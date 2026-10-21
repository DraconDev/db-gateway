import { access, chmod, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { constants, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { chromium } from 'playwright';

export const repoRoot = resolve(fileURLToPath(new URL('../../', import.meta.url)));
export const gatewayBinary = join(repoRoot, '.cargo/target/release/dg');

export function run(command, args, { cwd = repoRoot, timeoutMs = 900_000 } = {}) {
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(command, args, { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      rejectRun(new Error(`${command} ${args.join(' ')} timed out`));
    }, timeoutMs);
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.once('error', (error) => { clearTimeout(timer); rejectRun(error); });
    child.once('exit', (code, signal) => {
      clearTimeout(timer);
      if (code === 0) resolveRun({ stdout, stderr });
      else rejectRun(new Error(`${command} ${args.join(' ')} exited ${code ?? signal}\n${stderr || stdout}`));
    });
  });
}

async function freePort() {
  return new Promise((resolvePort, rejectPort) => {
    const probe = createServer();
    probe.once('error', rejectPort);
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address();
      const port = typeof address === 'object' && address ? address.port : 0;
      probe.close((error) => error ? rejectPort(error) : resolvePort(port));
    });
  });
}

async function terminate(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((resolveExit) => {
    let settled = false;
    const finish = () => { if (!settled) { settled = true; clearTimeout(timer); resolveExit(); } };
    const timer = setTimeout(() => {
      if (child.exitCode === null) child.kill('SIGKILL');
      finish();
    }, 3_000);
    child.once('exit', finish);
    child.kill('SIGTERM');
  });
}

async function waitForLog(child, predicate, timeoutMs = 15_000) {
  return new Promise((resolveLog, rejectLog) => {
    let output = '';
    let settled = false;
    const finish = (fn, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      fn(value);
    };
    const observe = (chunk) => {
      output += chunk;
      const value = predicate(output);
      if (value) finish(resolveLog, { value, output });
    };
    const timer = setTimeout(() => finish(rejectLog, new Error(`process readiness timeout: ${output}`)), timeoutMs);
    child.stdout?.on('data', observe);
    child.stderr?.on('data', observe);
    child.once('error', (error) => finish(rejectLog, error));
    child.once('exit', (code, signal) => finish(rejectLog, new Error(`process exited ${code ?? signal}: ${output}`)));
  });
}

function addressFromRustLog(text) {
  for (const line of text.split('\n')) {
    try {
      const value = JSON.parse(line);
      if (value.fields?.address) return value.fields.address;
    } catch { /* the gateway log is line-delimited JSON */ }
  }
  return undefined;
}

export async function startStack({ build = true } = {}) {
  if (build) await run('cargo', ['build', '--release', '--locked', '--quiet']);
  await access(gatewayBinary, constants.X_OK);
  const root = await mkdtemp(join(tmpdir(), 'dg-browser-stack-'));
  const data = join(root, 'data');
  const keyFile = join(root, 'owner.key');
  const init = await run(gatewayBinary, ['init', '--data-dir', data]);
  const ownerKey = JSON.parse(init.stdout).owner_key;
  if (!/^[a-f0-9]{64}$/.test(ownerKey)) throw new Error('fixture owner key is malformed');
  await writeFile(keyFile, `${ownerKey}\n`, { mode: 0o600 });
  await chmod(keyFile, 0o600);

  const gateway = spawn(gatewayBinary, ['serve', '--listen', '127.0.0.1:0', '--data-dir', data], { stdio: ['ignore', 'pipe', 'pipe'] });
  const gatewayReady = await waitForLog(gateway, addressFromRustLog);
  const gatewayAddress = gatewayReady.value;
  const gatewayBase = `http://${gatewayAddress}`;

  const consolePort = await freePort();
  const consoleProcess = spawn(process.execPath, [join(repoRoot, 'web/dist/server.js')], {
    env: {
      ...process.env,
      DG_CONSOLE_HOST: '127.0.0.1',
      DG_CONSOLE_PORT: String(consolePort),
      DG_SERVER: gatewayBase,
      DG_KEY_FILE: keyFile,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const consoleReady = await waitForLog(consoleProcess, (text) => text.includes('console listening'));
  const consoleBase = `http://127.0.0.1:${consolePort}`;
  const logs = {
    gateway: gatewayReady.output,
    companion: consoleReady.output,
  };
  const collect = (child, name) => {
    const append = (chunk) => { logs[name] += chunk; };
    child.stdout.on('data', append);
    child.stderr.on('data', append);
  };
  collect(gateway, 'gateway');
  collect(consoleProcess, 'companion');

  return {
    root,
    data,
    keyFile,
    ownerKey,
    gateway,
    gatewayBase,
    consoleProcess,
    consoleBase,
    logs,
    async close() {
      await terminate(consoleProcess);
      await terminate(gateway);
      await rm(root, { recursive: true, force: true });
    },
  };
}

function browserExecutable() {
  const candidates = [
    process.env.PLAYWRIGHT_BROWSER_EXECUTABLE,
    '/run/current-system/sw/bin/chromium',
    '/usr/bin/chromium',
    '/usr/bin/google-chrome',
  ].filter(Boolean);
  for (const candidate of candidates) if (existsSync(candidate)) return candidate;
  return undefined;
}

export async function launchBrowser() {
  const executablePath = browserExecutable();
  return chromium.launch({
    headless: true,
    ...(executablePath ? { executablePath } : {}),
    args: ['--no-sandbox'],
  });
}

export async function openConsole(stack, browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  const page = await context.newPage();
  const browserErrors = [];
  const requests = [];
  // Chromium requests /favicon.ico by default. The console answers it
  // with 204 (web/src/server.ts), so it is not an error at all -- and
  // nothing else may be: the old whitelist also dropped every
  // "404 (Not Found)" message, which is exactly what a broken console
  // route produces, so this assertion could no longer catch one.
  const isExpectedResourceError = (message) => message.includes('favicon');
  page.on('pageerror', (error) => browserErrors.push(error.message));
  page.on('console', (message) => { if (message.type() === 'error' && !isExpectedResourceError(message.text())) browserErrors.push(message.text()); });
  page.on('request', (request) => requests.push({ url: request.url(), method: request.method(), headers: request.headers(), body: request.postData() }));
  await page.goto(`${stack.consoleBase}/`, { waitUntil: 'networkidle' });
  return { context, page, browserErrors, requests };
}
