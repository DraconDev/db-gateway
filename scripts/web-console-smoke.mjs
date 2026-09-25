#!/usr/bin/env node
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { spawn } from 'node:child_process';

const root = new URL('../', import.meta.url).pathname;
const data = await mkdtemp(join(tmpdir(), 'dg-web-smoke-'));
const keyFile = join(data, 'owner.key');
const init = spawn(join(root, 'target/release/dg'), ['init', '--data-dir', data], { encoding: 'utf8' });
let initOut = ''; let initErr = ''; init.stdout.on('data', (x) => { initOut += x; }); init.stderr.on('data', (x) => { initErr += x; });
await new Promise((resolve, reject) => { init.on('error', reject); init.on('exit', (c) => c ? reject(new Error(initErr || initOut)) : resolve()); });
const key = JSON.parse(initOut).owner_key;
await writeFile(keyFile, key + '\n'); await chmod(keyFile, 0o600);
const server = spawn(join(root, 'target/release/dg'), ['serve', '--listen', '127.0.0.1:0', '--data-dir', data], { stdio: ['ignore', 'ignore', 'pipe'] });
const address = await new Promise((resolve, reject) => { let b = ''; const timer = setTimeout(() => reject(new Error('server readiness timeout')), 5000); server.stderr.on('data', (x) => { b += x; for (const line of b.split('\n')) { try { const j = JSON.parse(line); if (j.fields?.address) { clearTimeout(timer); resolve(j.fields.address); } } catch {} } }); server.on('exit', (c) => reject(new Error(`server exited ${c}`))); });
const base = `http://${address}`;
async function call(path, method = 'GET', value) { const r = await fetch(base + path, { method, headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' }, body: value === undefined ? undefined : JSON.stringify(value) }); const j = await r.json(); assert.equal(r.ok, true, `${method} ${path}: ${JSON.stringify(j)}`); return j; }
async function companion(path, options = {}) { const r = await fetch(`http://127.0.0.1:${options.port ?? 0}${path}`, options); const text = await r.text(); let value; try { value = JSON.parse(text); } catch { value = { raw: text }; } return { status: r.status, value, headers: r.headers }; }
try {
  assert.equal((await call('/health/live')).status, 'ok');
  const created = await call('/v1/databases', 'POST', { name: 'smoke' }); assert.equal(created.name, 'smoke');
  const info = await call('/v1/databases/smoke'); assert.equal(info.name, 'smoke');
  const q = await call('/v1/databases/smoke/query', 'POST', { sql: 'CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)' }); assert.equal(q.rows_affected, 0);
  const status = await call('/v1/status'); assert.equal(typeof status.uptime_secs, 'number');
  const backups = await call('/v1/backups'); assert.ok(Array.isArray(backups.sets));
  const { spawn: spawnNode } = await import('node:child_process');
  const companionPort = 18787 + Math.floor(Math.random() * 1000);
  const consoleProcess = spawnNode(process.execPath, [join(root, 'web/dist/server.js')], { env: { ...process.env, DG_SERVER: base, DG_KEY_FILE: keyFile, DG_CONSOLE_PORT: String(companionPort) }, stdio: ['ignore', 'pipe', 'pipe'] });
  await new Promise((resolve, reject) => { let output = ''; const timer = setTimeout(() => reject(new Error(`console readiness timeout: ${output}`)), 5000); const observe = (x) => { output += x; if (output.includes('console listening')) { clearTimeout(timer); resolve(); } }; consoleProcess.stdout.on('data', observe); consoleProcess.stderr.on('data', observe); consoleProcess.on('error', reject); consoleProcess.on('exit', (code) => reject(new Error(`console exited ${code}: ${output}`))); });
  const html = await companion('/', { port: companionPort }); assert.equal(html.status, 200); assert.match(String(html.value.raw), /DB Gateway/);
  const sessionCookie = String(html.headers.get('set-cookie') ?? '').split(';')[0];
  const overview = await companion('/api/overview', { port: companionPort, headers: { cookie: sessionCookie } }); assert.equal(overview.status, 200); assert.equal(overview.value.gateway.available, true);
  consoleProcess.kill('SIGTERM'); await new Promise((resolve) => consoleProcess.on('exit', resolve));
  console.log(JSON.stringify({ ok: true, address, checks: ['health', 'create', 'info', 'query', 'status', 'backups', 'companion_overview', 'companion_html'] }));
} finally { server.kill('SIGTERM'); await new Promise((resolve) => server.on('exit', resolve)); await rm(data, { recursive: true, force: true }); }
