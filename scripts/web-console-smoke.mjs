#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { access, chmod, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';

const root = resolve(fileURLToPath(new URL('../', import.meta.url)));
const binary = join(root, 'target/release/dg');
const node = process.execPath;

function run(command, args, { cwd = root, timeoutMs = 900_000 } = {}) {
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
      else rejectRun(new Error(`${command} ${args.join(' ')} exited ${code ?? signal}\n${stderr || 'command failed'}`));
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
    }, 2_000);
    child.once('exit', finish);
    child.kill('SIGTERM');
  });
}

async function waitForAddress(child, timeoutMs = 10_000) {
  return new Promise((resolveAddress, rejectAddress) => {
    let output = '';
    const timer = setTimeout(() => rejectAddress(new Error(`server readiness timeout: ${output}`)), timeoutMs);
    child.stderr.on('data', (chunk) => {
      output += chunk;
      for (const line of output.split('\n')) {
        try {
          const value = JSON.parse(line);
          if (value.fields?.address) {
            clearTimeout(timer);
            resolveAddress(value.fields.address);
            return;
          }
        } catch { /* keep reading the structured log stream */ }
      }
    });
    child.once('error', (error) => { clearTimeout(timer); rejectAddress(error); });
    child.once('exit', (code, signal) => { clearTimeout(timer); rejectAddress(new Error(`server exited ${code ?? signal}: ${output}`)); });
  });
}

async function waitForCompanion(child, timeoutMs = 10_000) {
  return new Promise((resolveReady, rejectReady) => {
    let output = '';
    const timer = setTimeout(() => rejectReady(new Error(`console readiness timeout: ${output}`)), timeoutMs);
    const observe = (chunk) => {
      output += chunk;
      if (output.includes('console listening')) { clearTimeout(timer); resolveReady(); }
    };
    child.stdout.on('data', observe);
    child.stderr.on('data', observe);
    child.once('error', (error) => { clearTimeout(timer); rejectReady(error); });
    child.once('exit', (code, signal) => { clearTimeout(timer); rejectReady(new Error(`console exited ${code ?? signal}: ${output}`)); });
  });
}

await run('cargo', ['build', '--release', '--locked', '--quiet']);
await access(binary, constants.X_OK);

const data = await mkdtemp(join(tmpdir(), 'dg-web-smoke-'));
const keyFile = join(data, 'owner.key');
let server;
let consoleProcess;
try {
  const init = await run(binary, ['init', '--data-dir', data]);
  const key = JSON.parse(init.stdout).owner_key;
  assert.match(key, /^[a-f0-9]{64}$/);
  await writeFile(keyFile, `${key}\n`);
  await chmod(keyFile, 0o600);

  server = spawn(binary, ['serve', '--listen', '127.0.0.1:0', '--data-dir', data], { stdio: ['ignore', 'ignore', 'pipe'] });
  const address = await waitForAddress(server);
  const base = `http://${address}`;

  async function call(path, method = 'GET', value) {
    const response = await fetch(base + path, {
      method,
      headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' },
      body: value === undefined ? undefined : JSON.stringify(value),
    });
    const text = await response.text();
    let parsed;
    try { parsed = text ? JSON.parse(text) : null; } catch { parsed = { raw: text }; }
    assert.equal(response.ok, true, `${method} ${path}: ${JSON.stringify(parsed)}`);
    return parsed;
  }

  assert.equal((await call('/health/live')).status, 'ok');
  assert.equal((await call('/v1/databases', 'POST', { name: 'smoke' })).name, 'smoke');
  assert.equal((await call('/v1/databases/smoke/query', 'POST', { sql: 'CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)' })).rows_affected, 0);
  assert.equal((await call('/v1/databases/smoke/query', 'POST', { sql: 'INSERT INTO t(v) VALUES (?)', args: ['seed'] })).rows_affected, 1);
  const largeValue = 'x'.repeat(40 * 1024);
  assert.equal((await call('/v1/databases/smoke/query', 'POST', { sql: 'INSERT INTO t(v) VALUES (?)', args: [largeValue] })).rows_affected, 1);
  const info = await call('/v1/databases/smoke');
  assert.equal(info.name, 'smoke');
  assert.ok(Array.isArray(info.tables));
  assert.equal((await call('/v1/status')).uptime_secs >= 0, true);
  assert.ok(Array.isArray((await call('/v1/backups')).sets));

  const consolePort = 18_787 + Math.floor(Math.random() * 1_000);
  const companionBase = `http://127.0.0.1:${consolePort}`;
  consoleProcess = spawn(node, [join(root, 'web/dist/server.js')], {
    env: { ...process.env, DG_SERVER: base, DG_KEY_FILE: keyFile, DG_CONSOLE_PORT: String(consolePort) },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  await waitForCompanion(consoleProcess);

  async function companion(path, { method = 'GET', body, headers = {}, expected = 200 } = {}) {
    const requestHeaders = { cookie: sessionCookie, origin: companionBase, ...headers };
    if (body !== undefined) requestHeaders['content-type'] = 'application/json';
    const response = await fetch(companionBase + path, { method, headers: requestHeaders, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await response.text();
    let value;
    try { value = text ? JSON.parse(text) : null; } catch { value = { raw: text }; }
    assert.equal(response.status, expected, `${method} ${path}: ${JSON.stringify(value)}`);
    return value;
  }

  const htmlResponse = await fetch(companionBase + '/');
  assert.equal(htmlResponse.status, 200);
  const html = await htmlResponse.text();
  assert.match(html, /DB Gateway/);
  assert.match(html, /id="import-form"/);
  const sessionCookie = (htmlResponse.headers.get('set-cookie') ?? '').split(';')[0];
  assert.match(sessionCookie, /^dg_console_session=/);

  const overview = await companion('/api/overview');
  assert.equal(overview.gateway.available, true);
  assert.ok(overview.resource_signals.some((signal) => signal.state === 'unavailable'));
  assert.ok(overview.resource_signals.some((signal) => signal.state === 'not_configured'));

  const detail = await companion('/api/databases/smoke');
  assert.equal(detail.name, 'smoke');
  assert.equal(detail.schema.columns.includes('name'), true);
  assert.ok(detail.tables.some((table) => table.name === 't'));

  assert.equal((await companion('/api/databases/smoke/query', { method: 'POST', body: { sql: 'SELECT count(*) FROM t', args: [] } })).rows[0][0], 2);
  assert.equal((await companion('/api/databases/smoke/batch', { method: 'POST', body: { atomic: true, statements: [{ sql: 'SELECT count(*) FROM t' }] } })).results.length, 1);

  const invalidName = await companion('/api/databases/UPPER', { expected: 400 });
  assert.equal(invalidName.error.code, 'invalid_request');
  const invalidArgs = await companion('/api/databases/smoke/query', { method: 'POST', body: { sql: 'SELECT 1', args: [{}] }, expected: 400 });
  assert.equal(invalidArgs.error.code, 'invalid_request');

  const issued = await companion('/api/keys', { method: 'POST', body: { name: 'smoke_key', role: 'database_read_only', databases: ['smoke'] } });
  assert.match(issued.key, /^[a-f0-9]{64}$/);
  assert.ok((await companion('/api/keys')).keys.some((item) => item.name === 'smoke_key'));
  await companion(`/api/keys/smoke_key`, { method: 'DELETE' });
  assert.equal((await companion('/api/keys')).keys.some((item) => item.name === 'smoke_key'), false);

  const cloned = await companion('/api/databases/smoke/clone', { method: 'POST', body: { destination: 'smoke_copy' } });
  assert.equal(cloned.destination, 'smoke_copy');

  const singleExport = await companion('/api/databases/smoke/export', { method: 'POST', body: {} });
  assert.match(singleExport.sha256, /^[a-f0-9]{64}$/);
  assert.equal(singleExport.size > 0, true);

  const exportBegin = await companion('/api/databases/smoke/export-begin', { method: 'POST', body: {} });
  assert.match(exportBegin.snapshot, /^[a-f0-9]{64}$/);
  assert.ok(exportBegin.size > 0);
  const exportedParts = [];
  let exportOffset = 0;
  while (exportOffset < exportBegin.size) {
    const length = Math.min(32 * 1024, exportBegin.size - exportOffset);
    const chunk = await companion(`/api/snapshots/${exportBegin.snapshot}/chunk?offset=${exportOffset}&length=${length}`);
    assert.equal(chunk.offset, exportOffset);
    assert.equal(chunk.size, exportBegin.size);
    assert.equal(chunk.sha256, exportBegin.sha256);
    const part = Buffer.from(chunk.data_base64, 'base64');
    assert.ok(part.length > 0 && part.length <= length);
    exportedParts.push(part);
    exportOffset += part.length;
  }
  assert.equal(exportOffset, exportBegin.size);
  const exportedBytes = Buffer.concat(exportedParts);
  assert.equal(createHash('sha256').update(exportedBytes).digest('hex'), exportBegin.sha256);
  await companion(`/api/snapshots/${exportBegin.snapshot}`, { method: 'DELETE' });

  const importBegin = await companion('/api/databases/console/import-begin', { method: 'POST', body: {} });
  assert.match(importBegin.upload, /^[a-f0-9]{64}$/);
  let uploadOffset = 0;
  let statusChecked = false;
  while (uploadOffset < exportedBytes.length) {
    const end = Math.min(uploadOffset + 32 * 1024, exportedBytes.length);
    const ack = await companion(`/api/uploads/${importBegin.upload}/chunk`, { method: 'POST', body: { offset: uploadOffset, data_base64: exportedBytes.subarray(uploadOffset, end).toString('base64') } });
    assert.equal(ack.received, end);
    uploadOffset = end;
    if (!statusChecked) {
      const status = await companion(`/api/uploads/${importBegin.upload}`);
      assert.equal(status.received, uploadOffset);
      statusChecked = true;
    }
  }
  assert.equal((await companion(`/api/uploads/${importBegin.upload}`)).received, exportedBytes.length);
  assert.equal((await companion(`/api/uploads/${importBegin.upload}/commit`, { method: 'POST', body: { name: 'smoke_imported' } })).imported, 'smoke_imported');
  assert.equal((await companion('/api/databases/smoke_imported/query', { method: 'POST', body: { sql: 'SELECT count(*) FROM t', args: [] } })).rows[0][0], 2);

  const abortBegin = await companion('/api/databases/console/import-begin', { method: 'POST', body: {} });
  await companion(`/api/uploads/${abortBegin.upload}/chunk`, { method: 'POST', body: { offset: 0, data_base64: exportedBytes.subarray(0, 1024).toString('base64') } });
  await companion(`/api/uploads/${abortBegin.upload}`, { method: 'DELETE' });
  const abortedStatus = await companion(`/api/uploads/${abortBegin.upload}`, { expected: 404 });
  assert.equal(abortedStatus.error.code, 'upload_not_found');

  assert.equal((await companion('/api/databases/smoke_copy', { method: 'DELETE' })).deleted, true);
  assert.equal((await companion('/api/databases/smoke_imported', { method: 'DELETE' })).deleted, true);

  const appSource = await (await fetch(companionBase + '/app.js')).text();
  assert.match(appSource, /file\.size > MAX_TRANSFER_BYTES/);
  assert.match(appSource, /offset \+= part\.length/);
  assert.match(appSource, /Resume import/);
  assert.match(appSource, /Abort upload/);

  assert.equal((await companion('/api/tunnel/status')).state, 'not_configured');
  assert.equal((await companion('/api/tunnel/start', { method: 'POST', body: {} })).state, 'not_configured');
  assert.equal((await companion('/api/tunnel/stop', { method: 'POST', body: {} })).state, 'not_configured');

  console.log(JSON.stringify({ ok: true, address, checks: ['build_release', 'health', 'create', 'detail_schema', 'query', 'batch', 'typed_errors', 'key_issue_revoke', 'clone', 'single_export', 'chunked_export_status', 'chunked_import_resume_status', 'upload_abort', 'delete', 'browser_controls', 'tunnel_status_start_stop'] }));
} finally {
  await terminate(consoleProcess);
  await terminate(server);
  await rm(data, { recursive: true, force: true });
}
