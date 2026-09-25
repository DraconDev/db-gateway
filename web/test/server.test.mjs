import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { request as httpRequest, createServer as createHttpServer } from 'node:http';
import { serve } from '../dist/server.js';

const key = 'a'.repeat(64);

async function withServer(run, overrides = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'dg-web-server-'));
  const keyFile = join(dir, 'owner.key');
  await writeFile(keyFile, key + '\n');
  await chmod(keyFile, 0o600);
  const instance = await serve({ host: '127.0.0.1', port: 0, gatewayUrl: 'http://127.0.0.1:1', keyFile, ...overrides });
  const address = instance.server.address();
  assert.equal(typeof address, 'object');
  const port = address.port;
  try { await run(`http://127.0.0.1:${port}`); }
  finally { await instance.close(); await rm(dir, { recursive: true, force: true }); }
}

function rawGet(base, path) {
  const url = new URL(base);
  return new Promise((resolve, reject) => {
    const request = httpRequest({ hostname: url.hostname, port: url.port, path, method: 'GET' }, (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => resolve({ status: response.statusCode, body: Buffer.concat(chunks).toString('utf8') }));
    });
    request.once('error', reject);
    request.end();
  });
}

test('console serves a session cookie and protects companion API', async () => {
  await withServer(async (base) => {
    const page = await fetch(base + '/');
    assert.equal(page.status, 200);
    const cookie = (page.headers.get('set-cookie') ?? '').split(';')[0];
    assert.match(cookie, /^dg_console_session=/);
    assert.equal((await fetch(base + '/api/status')).status, 401);
    assert.equal((await fetch(base + '/api/status', { headers: { cookie } })).status, 503);
    const origin = await fetch(base + '/api/status', { headers: { cookie, origin: 'http://evil.example' } });
    assert.equal(origin.status, 403);
  });
});

test('malformed request paths return a structured 400 without terminating the companion', async () => {
  await withServer(async (base) => {
    const malformed = await rawGet(base, '/%E0%A4%A');
    assert.equal(malformed.status, 400);
    assert.deepEqual(JSON.parse(malformed.body), { error: { code: 'invalid_request', message: 'Malformed request URL.' } });
    const after = await fetch(base + '/');
    assert.equal(after.status, 200);
  });
});

test('serve rejects non-loopback listener hosts at the runtime bind boundary', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'dg-web-bind-'));
  const keyFile = join(dir, 'owner.key');
  await writeFile(keyFile, key + '\n');
  await chmod(keyFile, 0o600);
  try {
    for (const host of ['0.0.0.0', '::', '192.0.2.10']) {
      await assert.rejects(serve({ host, port: 0, gatewayUrl: 'http://127.0.0.1:1', keyFile }), /Console host must be loopback/);
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('redacts upstream errors and extra fields at the companion response boundary', async () => {
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
  const gatewayAddress = gateway.address();
  assert.equal(typeof gatewayAddress, 'object');
  try {
    await withServer(async (base) => {
      const page = await fetch(base + '/');
      const cookie = (page.headers.get('set-cookie') ?? '').split(';')[0];
      const errorResponse = await fetch(base + '/api/status', { headers: { cookie } });
      const errorBody = await errorResponse.text();
      assert.equal(errorResponse.status, 500);
      assert.equal(errorBody.includes(key), false);
      assert.match(errorBody, /\[REDACTED\]/);
      mode = 'success';
      const successResponse = await fetch(base + '/api/status', { headers: { cookie } });
      const successBody = await successResponse.text();
      assert.equal(successResponse.status, 200);
      assert.equal(successBody.includes(key), false);
      assert.match(successBody, /\[REDACTED\]/);
    }, { gatewayUrl: `http://127.0.0.1:${gatewayAddress.port}` });
  } finally {
    await new Promise((resolve, reject) => gateway.close((error) => error ? reject(error) : resolve()));
  }
});
