import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { chromium } from 'playwright';

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
