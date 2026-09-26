#!/usr/bin/env node
// Disposable end-to-end smoke for `dg web`: init + serve a temp gateway,
// seed one database, launch the installed/candidate dg's `web` subcommand
// with default companion discovery, then assert the dashboard serves real
// data, the owner key leaks nowhere, and SIGTERM stops both processes.
//
// Usage: node scripts/dg-web-smoke.mjs [--dg PATH] [--port N]
// Exit 0 with a JSON summary on success; nonzero with diagnostics on failure.
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createServer } from 'node:net';
import { spawn, execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = resolve(fileURLToPath(new URL('../', import.meta.url)));
const targetDir = process.env.CARGO_TARGET_DIR ?? join(root, '.cargo', 'target');
const args = process.argv.slice(2);
let dg = join(targetDir, 'release', 'dg');
let explicitDg = false;
let port = 0;
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--dg') { dg = args[++i] ?? ''; explicitDg = true; }
  else if (args[i] === '--port') port = Number(args[++i]);
  else if (args[i] === '--help') {
    console.log('Usage: node scripts/dg-web-smoke.mjs [--dg PATH] [--port N]');
    process.exit(0);
  } else {
    console.error(`unknown argument: ${args[i]}`);
    process.exit(2);
  }
}
if (!dg) { console.error('missing value for --dg'); process.exit(2); }
if (!Number.isInteger(port) || port < 0 || port > 65535) { console.error('--port must be 0-65535'); process.exit(2); }

async function freePort() {
  return new Promise((resolvePort, rejectPort) => {
    const probe = createServer();
    probe.once('error', rejectPort);
    probe.listen(0, '127.0.0.1', () => {
      const picked = probe.address().port;
      probe.close(() => resolvePort(picked));
    });
  });
}
if (port === 0) port = await freePort();
if (port < 1024) { console.error(`picked privileged port ${port}; pass --port explicitly`); process.exit(2); }
if (!explicitDg) {
  // Never smoke a stale binary: a rebuild is a no-op when fresh, and an
  // explicit --dg always means "test exactly this artifact".
  try {
    execFileSync('cargo', ['build', '--release', '--locked', '--quiet', '--bin', 'dg'], { cwd: root, stdio: ['ignore', 'pipe', 'pipe'] });
  } catch (error) {
    console.error(`release build failed: ${error.message}`);
    process.exit(1);
  }
}

const dir = await mkdtemp(join(tmpdir(), 'dg-web-smoke-'));
const children = [];
async function killAll() {
  for (const child of children) {
    if (child.exitCode === null) {
      child.kill('SIGTERM');
      await new Promise((resolveExit) => {
        const timer = setTimeout(() => { try { child.kill('SIGKILL'); } catch {} resolveExit(); }, 3000);
        child.once('exit', () => { clearTimeout(timer); resolveExit(); });
      });
    }
  }
}
process.on('exit', () => { for (const child of children) { try { child.kill('SIGKILL'); } catch {} } });
function fail(check, detail) {
  console.error(JSON.stringify({ ok: false, failed_check: check, detail: String(detail ?? '').slice(0, 2000) }));
  process.exitCode = 1;
  throw new Error(check);
}

try {
  const initRaw = execFileSync(dg, ['init', '--data-dir', dir], { encoding: 'utf8' });
  const key = JSON.parse(initRaw).owner_key;
  const keyFile = join(dir, 'key');
  await writeFile(keyFile, key);
  await chmod(keyFile, 0o600);

  const gateway = spawn(dg, ['serve', '--listen', '127.0.0.1:0', '--data-dir', dir], { stdio: ['ignore', 'pipe', 'pipe'] });
  children.push(gateway);
  const gatewayAddr = await new Promise((resolveAddr, rejectAddr) => {
    let out = '';
    const timer = setTimeout(() => rejectAddr(new Error('gateway readiness timeout: ' + out.slice(-500))), 20000);
    gateway.stderr.on('data', (chunk) => {
      out += chunk;
      for (const line of out.split('\n')) {
        try {
          const value = JSON.parse(line);
          if (value.fields?.address) { clearTimeout(timer); resolveAddr(value.fields.address); return; }
        } catch { /* keep reading the structured log stream */ }
      }
    });
  });
  const created = await fetch(`http://${gatewayAddr}/v1/databases`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: 'smokedb' }),
  });
  if (!created.ok) fail('seed_create', await created.text());

  const web = spawn(dg, ['--server', `http://${gatewayAddr}`, '--key-file', keyFile, 'web', '--port', String(port)], {
    cwd: root,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  children.push(web);
  let webOut = '';
  web.stdout.on('data', (chunk) => { webOut += chunk; });
  web.stderr.on('data', (chunk) => { webOut += chunk; });
  const started = Date.now();
  while (!webOut.includes('dg web ready:') && Date.now() - started < 30000) {
    if (web.exitCode !== null) fail('web_ready', webOut.slice(-2000));
    await new Promise((r) => setTimeout(r, 200));
  }
  if (!webOut.includes('dg web ready:')) fail('web_ready', webOut.slice(-2000));

  const dashboard = await fetch(`http://127.0.0.1:${port}/`);
  const html = await dashboard.text();
  if (dashboard.status !== 200) fail('dashboard_status', dashboard.status);
  const cookie = (dashboard.headers.get('set-cookie') ?? '').split(';')[0];
  const overview = await fetch(`http://127.0.0.1:${port}/api/overview`, { headers: { Cookie: cookie } });
  const overviewText = await overview.text();
  if (overview.status !== 200) fail('overview_status', overview.status);
  if (!overviewText.includes('smokedb')) fail('overview_data', overviewText.slice(0, 500));
  if (html.includes(key)) fail('key_in_html', null);
  if (overviewText.includes(key)) fail('key_in_overview', null);
  if (webOut.includes(key)) fail('key_in_web_output', null);

  web.kill('SIGTERM');
  const webExit = await new Promise((resolveExit) => {
    const timer = setTimeout(() => resolveExit('TIMEOUT'), 15000);
    web.once('exit', (code) => { clearTimeout(timer); resolveExit(code); });
  });
  if (webExit !== 0) fail('web_exit', webExit);
  let portClosed = false;
  try { await fetch(`http://127.0.0.1:${port}/`); } catch { portClosed = true; }
  if (!portClosed) fail('companion_port_closed', null);

  console.log(JSON.stringify({ ok: true, dg, port, checks: ['seed_create', 'web_ready', 'dashboard_status', 'overview_status', 'overview_data', 'key_in_html', 'key_in_overview', 'key_in_web_output', 'web_exit', 'companion_port_closed'] }));
} catch (error) {
  if (process.exitCode !== 1) {
    console.error(JSON.stringify({ ok: false, failed_check: 'unexpected', detail: String(error?.message ?? error).slice(0, 2000) }));
    process.exitCode = 1;
  }
} finally {
  await killAll();
  await rm(dir, { recursive: true, force: true });
}
