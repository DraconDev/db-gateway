import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { createServer as createHttpServer } from 'node:http';
import { chromium } from 'playwright';
import { resolveChromium } from '../scripts/resolve-chromium.mjs';

const key = 'b'.repeat(64);
const root = new URL('../../', import.meta.url).pathname;
const distServer = join(root, 'web/dist/server.js');

async function freePort() {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once('error', reject);
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address();
      probe.close((error) => error ? reject(error) : resolve(typeof address === 'object' && address ? address.port : 0));
    });
  });
}

async function terminate(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((resolve) => {
    const timer = setTimeout(() => { if (child.exitCode === null) child.kill('SIGKILL'); resolve(); }, 2_000);
    child.once('exit', () => { clearTimeout(timer); resolve(); });
    child.kill('SIGTERM');
  });
}

async function startAdversarialCompanion() {
  let mode = 'error';
  const gateway = createHttpServer((request, response) => {
    response.setHeader('content-type', 'application/json');
    if (mode === 'error') {
      response.statusCode = 500;
      response.end(JSON.stringify({ error: { code: 'upstream_leak', message: `leaked ${key}` } }));
      return;
    }
    response.end(JSON.stringify({
      version: '1', uptime_secs: 0,
      admission: { acquire_timeout_ms: 1, read_permits: 1, write_permits: 1 },
      requests: { total: 0, capacity_exceeded: 0, query_deadline_exceeded: 0, internal_error: 0 },
      owner_key: key,
    }));
  });
  await new Promise((resolve, reject) => { gateway.once('error', reject); gateway.listen(0, '127.0.0.1', resolve); });
  const address = gateway.address();
  if (!address || typeof address === 'string') throw new Error('adversarial gateway did not bind');
  const dir = await mkdtemp(join(tmpdir(), 'dg-adversarial-companion-'));
  const keyFile = join(dir, 'owner.key');
  await writeFile(keyFile, `${key}\n`); await chmod(keyFile, 0o600);
  const port = await freePort();
  const child = spawn(process.execPath, [distServer], { env: { ...process.env, DG_CONSOLE_HOST: '127.0.0.1', DG_CONSOLE_PORT: String(port), DG_SERVER: `http://127.0.0.1:${address.port}`, DG_KEY_FILE: keyFile }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (chunk) => { output += chunk; });
  child.stderr.on('data', (chunk) => { output += chunk; });
  const deadline = Date.now() + 10_000;
  while (!output.includes('console listening') && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 25));
  if (!output.includes('console listening')) throw new Error(`companion did not start: ${output}`);
  return {
    base: `http://127.0.0.1:${port}`,
    get output() { return output; },
    setMode(value) { mode = value; },
    async close() { await terminate(child); await new Promise((resolve, reject) => gateway.close((error) => error ? reject(error) : resolve())); await rm(dir, { recursive: true, force: true }); },
  };
}

test('adversarial upstream errors and extra fields never reach browser DOM, network, or logs', async () => {
  const stack = await startAdversarialCompanion();
  const browser = await chromium.launch({ headless: true, executablePath: process.env.PLAYWRIGHT_BROWSER_EXECUTABLE || '/run/current-system/sw/bin/chromium', args: ['--no-sandbox'] });
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    const requests = [];
    page.on('request', (request) => requests.push({ headers: request.headers(), body: request.postData() }));
    await page.goto(`${stack.base}/`, { waitUntil: 'networkidle' });
    const pageResponse = await fetch(`${stack.base}/`, { headers: { cookie: (await context.cookies()).map((value) => `dg_console_session=${value.value}`).join('; ') } });
    assert.equal(pageResponse.status, 200);
    const errorResponse = await page.request.get(`${stack.base}/api/status`);
    const errorBody = await errorResponse.text();
    assert.equal(errorBody.includes(key), false);
    assert.match(await page.locator('body').innerText(), /Signals unavailable|Connection/i);
    stack.setMode('success');
    const successResponse = await page.request.get(`${stack.base}/api/status`);
    const successBody = await successResponse.text();
    assert.equal(successBody.includes(key), false);
    const browserData = JSON.stringify({ html: await page.content(), storage: await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } })), requests });
    assert.equal(browserData.includes(key), false);
    assert.equal(requests.some((request) => Object.keys(request.headers).some((name) => name.toLowerCase() === 'authorization')), false);
    assert.equal(stack.output.includes(key), false);
    await context.close();
  } finally { await browser.close(); await stack.close(); }
});

test('behavioral security canary keeps the owner key server-side and binds only loopback', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'dg-security-canary-'));
  const keyFile = join(dir, 'owner.key');
  await writeFile(keyFile, `${key}\n`); await chmod(keyFile, 0o600);
  const port = await freePort();
  const processHandle = spawn(process.execPath, [distServer], { env: { ...process.env, DG_CONSOLE_HOST: '127.0.0.1', DG_CONSOLE_PORT: String(port), DG_SERVER: 'http://127.0.0.1:1', DG_KEY_FILE: keyFile }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  processHandle.stdout.on('data', (chunk) => { output += chunk; });
  processHandle.stderr.on('data', (chunk) => { output += chunk; });
  try {
    const deadline = Date.now() + 10_000;
    while (!output.includes('console listening') && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 25));
    assert.match(output, /DB Gateway console listening on http:\/\/127\.0\.0\.1:/);
    assert.equal(output.includes(key), false);
    const browser = await chromium.launch({ headless: true, executablePath: process.env.PLAYWRIGHT_BROWSER_EXECUTABLE || '/run/current-system/sw/bin/chromium', args: ['--no-sandbox'] });
    try {
      const context = await browser.newContext();
      const page = await context.newPage();
      const browserRequests = [];
      page.on('request', (request) => browserRequests.push({ headers: request.headers(), body: request.postData() }));
      await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: 'networkidle' });
      const browserData = JSON.stringify({ html: await page.content(), storage: await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage }, cookie: document.cookie })), requests: browserRequests });
      assert.equal(browserData.includes(key), false);
      assert.equal(browserRequests.some((request) => Object.keys(request.headers).some((name) => name.toLowerCase() === 'authorization')), false);
      assert.deepEqual(await context.cookies().then((cookies) => cookies.filter((cookie) => cookie.value.includes(key))), []);
      await context.close();
    } finally { await browser.close(); }
  } finally { await terminate(processHandle); await rm(dir, { recursive: true, force: true }); }
});
