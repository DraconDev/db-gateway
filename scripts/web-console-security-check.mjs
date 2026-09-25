#!/usr/bin/env node
import assert from 'node:assert/strict';
import { access, chmod, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { chromium } from 'playwright';

const root = resolve(fileURLToPath(new URL('../', import.meta.url)));
const distServer = join(root, 'web/dist/server.js');
const key = 'd'.repeat(64);
const files = ['web/src/server.ts', 'web/src/api.ts', 'web/public/app.js', 'web/public/index.html'];
for (const file of files) {
  const source = await import('node:fs/promises').then(({ readFile }) => readFile(join(root, file), 'utf8'));
  assert.doesNotMatch(source, /localStorage|sessionStorage|document\.cookie|Authorization: Bearer/, `${file} contains browser credential persistence/transport`);
  assert.doesNotMatch(source, /0\.0\.0\.0|\[::\]/, `${file} contains a non-loopback bind`);
}

async function freePort() {
  return new Promise((resolvePort, rejectPort) => {
    const probe = createServer();
    probe.once('error', rejectPort);
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address();
      probe.close((error) => error ? rejectPort(error) : resolvePort(typeof address === 'object' && address ? address.port : 0));
    });
  });
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((resolveExit) => {
    const timer = setTimeout(() => { if (child.exitCode === null) child.kill('SIGKILL'); resolveExit(); }, 2_000);
    child.once('exit', () => { clearTimeout(timer); resolveExit(); });
    child.kill('SIGTERM');
  });
}

const dir = await mkdtemp(join(root, '.security-canary-'));
const keyFile = join(dir, 'owner.key');
await writeFile(keyFile, `${key}\n`); await chmod(keyFile, 0o600);
const port = await freePort();
const child = spawn(process.execPath, [distServer], { env: { ...process.env, DG_CONSOLE_HOST: '127.0.0.1', DG_CONSOLE_PORT: String(port), DG_SERVER: 'http://127.0.0.1:1', DG_KEY_FILE: keyFile }, stdio: ['ignore', 'pipe', 'pipe'] });
let output = ''; child.stdout.on('data', (chunk) => { output += chunk; }); child.stderr.on('data', (chunk) => { output += chunk; });
try {
  const deadline = Date.now() + 10_000;
  while (!output.includes('console listening') && Date.now() < deadline) await new Promise((resolveWait) => setTimeout(resolveWait, 25));
  assert.match(output, /DB Gateway console listening on http:\/\/127\.0\.0\.1:/);
  assert.equal(output.includes(key), false);
  await access(distServer, constants.R_OK);
  const browser = await chromium.launch({ headless: true, executablePath: process.env.PLAYWRIGHT_BROWSER_EXECUTABLE || '/run/current-system/sw/bin/chromium', args: ['--no-sandbox'] });
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    const requests = [];
    page.on('request', (request) => requests.push({ headers: request.headers(), body: request.postData() }));
    await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: 'networkidle' });
    const browserData = JSON.stringify({ html: await page.content(), storage: await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage }, cookie: document.cookie })), requests });
    assert.equal(browserData.includes(key), false);
    assert.equal(requests.some((request) => Object.keys(request.headers).some((name) => name.toLowerCase() === 'authorization')), false);
    assert.deepEqual((await context.cookies()).filter((cookie) => cookie.value.includes(key)), []);
    await context.close();
  } finally { await browser.close(); }
  console.log(JSON.stringify({ ok: true, behavioral: true, owner_key_in_browser: false, non_loopback_bind: false, checked_files: files.length }));
} finally { await stop(child); await rm(dir, { recursive: true, force: true }); }
