import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { serve } from '../dist/server.js';

const key = 'a'.repeat(64);

async function withServer(run) {
  const dir = await mkdtemp(join(tmpdir(), 'dg-web-server-'));
  const keyFile = join(dir, 'owner.key');
  await writeFile(keyFile, key + '\n');
  await chmod(keyFile, 0o600);
  const instance = await serve({ host: '127.0.0.1', port: 0, gatewayUrl: 'http://127.0.0.1:1', keyFile });
  const address = instance.server.address();
  assert.equal(typeof address, 'object');
  const port = address.port;
  try { await run(`http://127.0.0.1:${port}`); }
  finally { await instance.close(); await rm(dir, { recursive: true, force: true }); }
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
