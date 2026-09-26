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

test('companion degrades list, overview, and detail joins when sections fail', async () => {
  const gateway = createHttpServer((request, response) => {
    response.setHeader('content-type', 'application/json');
    const fail = () => { response.statusCode = 500; response.end(JSON.stringify({ error: { code: 'boom', message: 'fake failure' } })); };
    if (request.url === '/v1/databases' && request.method === 'GET') {
      response.end(JSON.stringify({ databases: ['gooddb', 'baddb'] }));
    } else if (request.url === '/v1/databases/gooddb' && request.method === 'GET') {
      response.end(JSON.stringify({ name: 'gooddb', tombstoned: false, size_bytes: 512, created_at: 3, tables: [] }));
    } else if (request.url === '/v1/databases/baddb' && request.method === 'GET') {
      fail();
    } else if (request.url === '/v1/status' || request.url === '/v1/keys') {
      fail();
    } else if (request.url === '/v1/backups') {
      response.end(JSON.stringify({ backup_root: null, stale_after_secs: 300, sets: [], per_database: { gooddb: { state: 'fresh', newest_set: 's', age_secs: 1 } } }));
    } else if (request.url === '/health/live') {
      response.end(JSON.stringify({ status: 'ok' }));
    } else if (request.url === '/v1/databases/gooddb/query' && request.method === 'POST') {
      fail();
    } else {
      response.statusCode = 404;
      response.end(JSON.stringify({ error: { code: 'not_found', message: 'no such fake route' } }));
    }
  });
  await new Promise((resolve, reject) => { gateway.once('error', reject); gateway.listen(0, '127.0.0.1', resolve); });
  const gatewayAddress = gateway.address();
  assert.equal(typeof gatewayAddress, 'object');
  try {
    await withServer(async (base) => {
      const page = await fetch(base + '/');
      const cookie = (page.headers.get('set-cookie') ?? '').split(';')[0];
      const list = await fetch(base + '/api/databases', { headers: { cookie } });
      assert.equal(list.status, 200);
      assert.deepEqual(await list.json(), [
        {
          name: 'gooddb', size_bytes: 512, rows_read: null, rows_written: null,
          backup: { state: 'fresh', newest_set: 's', age_secs: 1 },
          tombstoned: false, created_at: 3, tables: [],
        },
        {
          name: 'baddb', unknown: true, size_bytes: null, rows_read: null, rows_written: null,
          backup: { state: 'unknown', newest_set: null, age_secs: null },
          tombstoned: false, tables: [],
        },
      ]);
      const overview = await fetch(base + '/api/overview', { headers: { cookie } });
      assert.equal(overview.status, 200);
      const body = await overview.json();
      assert.equal(body.gateway.available, true);
      assert.equal(body.gateway.version, undefined);
      assert.deepEqual(body.databases, ['gooddb', 'baddb']);
      assert.deepEqual(body.keys, []);
      const detail = await fetch(base + '/api/databases/gooddb', { headers: { cookie } });
      assert.equal(detail.status, 200);
      assert.deepEqual(await detail.json(), {
        name: 'gooddb', tombstoned: false, size_bytes: 512, created_at: 3, tables: [], schema: null,
      });
    }, { gatewayUrl: `http://127.0.0.1:${gatewayAddress.port}` });
  } finally {
    await new Promise((resolve, reject) => gateway.close((error) => error ? reject(error) : resolve()));
  }
});

test('companion single-shot export and import round-trip bounded payloads', async () => {
  const data = Buffer.from('fake-sqlite-bytes').toString('base64');
  const seen = [];
  const gateway = createHttpServer((request, response) => {
    let body = '';
    request.on('data', (chunk) => { body += chunk; });
    request.on('end', () => {
      response.setHeader('content-type', 'application/json');
      seen.push(`${request.method} ${request.url} ${body}`);
      if (request.url === '/v1/databases/smalldb/export' && request.method === 'POST') {
        response.end(JSON.stringify({ database: 'smalldb', size: 17, sha256: 'c'.repeat(64), data_base64: data }));
      } else if (request.url === '/v1/databases/console/import' && request.method === 'POST') {
        const input = JSON.parse(body);
        assert.equal(input.name, 'restoredb');
        assert.equal(input.data_base64, data);
        response.end(JSON.stringify({ imported: 'restoredb', from: 'console' }));
      } else {
        response.statusCode = 404;
        response.end(JSON.stringify({ error: { code: 'not_found', message: 'no such fake route' } }));
      }
    });
  });
  await new Promise((resolve, reject) => { gateway.once('error', reject); gateway.listen(0, '127.0.0.1', resolve); });
  const gatewayAddress = gateway.address();
  assert.equal(typeof gatewayAddress, 'object');
  try {
    await withServer(async (base) => {
      const page = await fetch(base + '/');
      const cookie = (page.headers.get('set-cookie') ?? '').split(';')[0];
      const exported = await fetch(base + '/api/databases/smalldb/export', { method: 'POST', headers: { cookie } });
      assert.equal(exported.status, 200);
      assert.deepEqual(await exported.json(), { database: 'smalldb', size: 17, sha256: 'c'.repeat(64), data_base64: data });
      const imported = await fetch(base + '/api/databases/ignored-label/import', {
        method: 'POST', headers: { cookie, 'content-type': 'application/json' },
        body: JSON.stringify({ name: 'restoredb', data_base64: data }),
      });
      assert.equal(imported.status, 200);
      assert.deepEqual(await imported.json(), { imported: 'restoredb', from: 'console' });
      assert.equal(seen.some((line) => line.startsWith('POST /v1/databases/smalldb/export')), true);
      assert.equal(seen.some((line) => line.startsWith('POST /v1/databases/console/import')), true);
    }, { gatewayUrl: `http://127.0.0.1:${gatewayAddress.port}` });
  } finally {
    await new Promise((resolve, reject) => gateway.close((error) => error ? reject(error) : resolve()));
  }
});

test('companion joins list, info, usage, and freshness into GET /api/databases', async () => {
  const gateway = createHttpServer((request, response) => {
    response.setHeader('content-type', 'application/json');
    if (request.url === '/v1/databases' && request.method === 'GET') {
      response.end(JSON.stringify({ databases: ['appdb'] }));
    } else if (request.url === '/v1/databases/appdb' && request.method === 'GET') {
      response.end(JSON.stringify({ name: 'appdb', tombstoned: false, size_bytes: 2048, created_at: 7, tables: [{ name: 't', rows: 3 }] }));
    } else if (request.url === '/v1/status') {
      response.end(JSON.stringify({
        version: '1', uptime_secs: 0,
        admission: { acquire_timeout_ms: 1, read_permits: 1, write_permits: 1 },
        requests: { total: 0, capacity_exceeded: 0, query_deadline_exceeded: 0, internal_error: 0 },
        databases: { appdb: { rows_read: 9, rows_written: 4 } },
      }));
    } else if (request.url === '/v1/backups') {
      response.end(JSON.stringify({ backup_root: null, stale_after_secs: 300, sets: [], per_database: { appdb: { state: 'fresh', newest_set: 's', age_secs: 1 } } }));
    } else if (request.url === '/v1/keys' && request.method === 'GET') {
      response.end(JSON.stringify({ keys: [] }));
    } else if (request.url === '/health/live') {
      response.end(JSON.stringify({ status: 'ok' }));
    } else {
      response.statusCode = 404;
      response.end(JSON.stringify({ error: { code: 'not_found', message: 'no such fake route' } }));
    }
  });
  await new Promise((resolve, reject) => { gateway.once('error', reject); gateway.listen(0, '127.0.0.1', resolve); });
  const gatewayAddress = gateway.address();
  assert.equal(typeof gatewayAddress, 'object');
  try {
    await withServer(async (base) => {
      const page = await fetch(base + '/');
      const cookie = (page.headers.get('set-cookie') ?? '').split(';')[0];
      const list = await fetch(base + '/api/databases', { headers: { cookie } });
      assert.equal(list.status, 200);
      assert.deepEqual(await list.json(), [{
        name: 'appdb', size_bytes: 2048, rows_read: 9, rows_written: 4,
        backup: { state: 'fresh', newest_set: 's', age_secs: 1 },
        tombstoned: false, created_at: 7, tables: [{ name: 't', rows: 3 }],
      }]);
      const overview = await fetch(base + '/api/overview', { headers: { cookie } });
      assert.equal(overview.status, 200);
      const body = await overview.json();
      assert.equal(body.server_url, `http://127.0.0.1:${gatewayAddress.port}`);
      assert.deepEqual(body.gateway.databases, { appdb: { rows_read: 9, rows_written: 4 } });
    }, { gatewayUrl: `http://127.0.0.1:${gatewayAddress.port}` });
  } finally {
    await new Promise((resolve, reject) => gateway.close((error) => error ? reject(error) : resolve()));
  }
});
