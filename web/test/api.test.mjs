import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EventEmitter } from 'node:events';
import { createHash } from 'node:crypto';
import { ApiError, GatewayApi, TransferError } from '../dist/api.js';
import { configFromEnv, logRequestFailure, readCredential } from '../dist/server.js';
import { startTunnel, stopTunnel, tunnelCommand, TunnelController } from '../dist/tunnel.js';

const key = 'a'.repeat(64);
const responseJson = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { 'content-type': 'application/json' } });
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

test('config and credentials fail closed', async () => {
  const old = { ...process.env };
  try {
    process.env.DG_CONSOLE_HOST = '0.0.0.0';
    assert.throws(() => configFromEnv(), /loopback/);
    process.env.DG_CONSOLE_HOST = '127.0.0.1';
    process.env.DG_CONSOLE_PORT = '80';
    assert.throws(() => configFromEnv(), /1024-65535/);
    const dir = await mkdtemp(join(tmpdir(), 'dg-web-credential-'));
    const file = join(dir, 'owner.key');
    await writeFile(file, key + '\n'); await chmod(file, 0o644);
    await assert.rejects(readCredential(file), /accessible/);
    await chmod(file, 0o600);
    assert.equal(await readCredential(file), key);
    await rm(dir, { recursive: true, force: true });
  } finally { process.env = old; }
});

test('gateway adapter preserves error codes and loopback restriction', async () => {
  assert.throws(() => new GatewayApi('http://example.com', key), /loopback/);
  const api = new GatewayApi('http://127.0.0.1:1', key);
  await assert.rejects(api.status(), (e) => e instanceof ApiError && e.code === 'transport_unavailable');
});

test('gateway adapter rejects malformed nested GET responses and malformed error envelopes', async () => {
  const originalFetch = globalThis.fetch;
  try {
    globalThis.fetch = async () => new Response(JSON.stringify({ version: 'x', uptime_secs: 1, admission: {}, requests: {} }), { status: 200, headers: { 'content-type': 'application/json' } });
    const api = new GatewayApi('http://127.0.0.1:8787', key);
    await assert.rejects(api.status(), (error) => error instanceof ApiError && error.code === 'invalid_gateway_response');
    globalThis.fetch = async () => new Response(JSON.stringify({ error: { code: 42 } }), { status: 503, headers: { 'content-type': 'application/json' } });
    await assert.rejects(api.status(), (error) => error instanceof ApiError && error.code === 'invalid_gateway_response');
  } finally { globalThis.fetch = originalFetch; }
});

test('gateway adapter redacts owner credentials from errors and successful payloads', async () => {
  const originalFetch = globalThis.fetch;
  try {
    globalThis.fetch = async () => new Response(JSON.stringify({ error: { code: `upstream_${key}`, message: `leaked ${key}` } }), { status: 500, headers: { 'content-type': 'application/json' } });
    const api = new GatewayApi('http://127.0.0.1:8787', key);
    await assert.rejects(api.status(), (error) => error instanceof ApiError && !error.code.includes(key) && !error.message.includes(key) && error.message.includes('[REDACTED]'));
    globalThis.fetch = async () => new Response(JSON.stringify({
      version: '1', uptime_secs: 0,
      admission: { acquire_timeout_ms: 1, read_permits: 1, write_permits: 1 },
      requests: { total: 0, capacity_exceeded: 0, query_deadline_exceeded: 0, internal_error: 0 },
      owner_key: key,
    }), { status: 200, headers: { 'content-type': 'application/json' } });
    const status = await api.status();
    assert.equal(JSON.stringify(status).includes(key), false);
    assert.equal(status.owner_key, '[REDACTED]');
  } finally { globalThis.fetch = originalFetch; }
});

test('request-failure logging redacts credentials before writing to stderr', () => {
  const originalError = console.error;
  const lines = [];
  console.error = (...values) => lines.push(values.map(String).join(' '));
  try { logRequestFailure(new Error(`unexpected failure leaked ${key}`), key); }
  finally { console.error = originalError; }
  assert.equal(lines.join('\n').includes(key), false);
  assert.match(lines.join('\n'), /\[REDACTED\]/);
});

test('local input assertions are typed as invalid requests', async () => {
  const api = new GatewayApi('http://127.0.0.1:8787', key);
  await assert.rejects(api.info('UPPER'), (error) => error instanceof ApiError && error.code === 'invalid_request' && error.status === 400);
  await assert.rejects(api.query('smoke', 'SELECT 1', [{}]), (error) => error instanceof ApiError && error.code === 'invalid_request' && error.status === 400);
  await assert.rejects(api.batch('smoke', [{ sql: 'SELECT 1', args: [{}] }]), (error) => error instanceof ApiError && error.code === 'invalid_request' && error.status === 400);
  await assert.rejects(api.importChunk('b'.repeat(64), 0, Buffer.alloc(1024 * 1024 + 1).toString('base64')), (error) => error instanceof ApiError && error.code === 'invalid_request' && error.status === 400);
});

test('chunked export reports decoded progress and releases a verified snapshot', async () => {
  const originalFetch = globalThis.fetch;
  const data = new Uint8Array(1024 * 1024 + 37);
  for (let i = 0; i < data.length; i++) data[i] = i % 251;
  const snapshot = 'c'.repeat(64);
  const digest = sha256(data);
  const progress = [];
  let released = false;
  try {
    globalThis.fetch = async (input, init) => {
      const url = new URL(input);
      if (init.method === 'POST' && url.pathname === '/v1/databases/large/export/begin') return responseJson({ snapshot, size: data.length, sha256: digest });
      if (init.method === 'GET' && url.pathname === `/v1/snapshots/${snapshot}`) {
        const offset = Number(url.searchParams.get('offset'));
        const length = Number(url.searchParams.get('length'));
        return responseJson({ snapshot, size: data.length, sha256: digest, offset, data_base64: Buffer.from(data.subarray(offset, offset + length)).toString('base64') });
      }
      if (init.method === 'DELETE' && url.pathname === `/v1/snapshots/${snapshot}`) { released = true; return responseJson({ released: true }); }
      throw new Error(`unexpected request ${init.method} ${url}`);
    };
    const api = new GatewayApi('http://127.0.0.1:8787', key);
    const result = await api.exportChunked('large', { onProgress: (value) => progress.push(value) });
    assert.deepEqual(result.data, data);
    assert.equal(result.sha256, digest);
    assert.equal(released, true);
    assert.deepEqual(progress.map((value) => value.transferred), [0, 1024 * 1024, data.length]);
  } finally { globalThis.fetch = originalFetch; }
});

test('chunked import reads status before sending and resumes a retained upload', async () => {
  const originalFetch = globalThis.fetch;
  const data = new Uint8Array(1024 * 1024 + 19);
  data.fill(7);
  const upload = 'b'.repeat(64);
  let received = 0;
  let statusReads = 0;
  let failOnce = true;
  const progress = [];
  try {
    globalThis.fetch = async (input, init) => {
      const url = new URL(input);
      if (init.method === 'POST' && url.pathname === '/v1/databases/console/import/begin') return responseJson({ upload });
      if (init.method === 'GET' && url.pathname === `/v1/uploads/${upload}`) { statusReads += 1; return responseJson({ upload, received, size_cap: 128 * 1024 * 1024 }); }
      if (init.method === 'POST' && url.pathname === `/v1/uploads/${upload}/chunk`) {
        if (failOnce) { failOnce = false; throw new TypeError('simulated transport failure'); }
        const body = JSON.parse(init.body);
        const bytes = Buffer.from(body.data_base64, 'base64');
        assert.equal(body.offset, received);
        received += bytes.length;
        return responseJson({ upload, received });
      }
      if (init.method === 'POST' && url.pathname === `/v1/uploads/${upload}/commit`) return responseJson({ imported: 'restored' });
      throw new Error(`unexpected request ${init.method} ${url}`);
    };
    const api = new GatewayApi('http://127.0.0.1:8787', key);
    await assert.rejects(api.importChunked('restored', data, 'console', { onProgress: (value) => progress.push(value) }), (error) => error instanceof TransferError && error.state.session.upload === upload && error.state.transferred === 0);
    assert.equal(statusReads, 1);
    const result = await api.resumeChunkedImport(upload, 'restored', data, 'console', { onProgress: (value) => progress.push(value) });
    assert.equal(result.imported, 'restored');
    assert.equal(received, data.length);
    assert.equal(statusReads, 2);
    assert.equal(progress.some((value) => value.transferred === data.length), true);
  } finally { globalThis.fetch = originalFetch; }
});

test('tunnel command is loopback-only and lifecycle is injectable', async () => {
  const spec = tunnelCommand({ target: 'owner@host', localPort: 17432, remotePort: 7432, identityFile: '/tmp/id' });
  assert.deepEqual(spec.args.slice(0, 5), ['-N', '-T', '-o', 'ExitOnForwardFailure=yes', '-L']);
  assert.match(spec.args[5], /^127\.0\.0\.1:17432:127\.0\.0\.1:7432$/);
  const child = new EventEmitter();
  child.pid = 123; child.exitCode = null; child.signalCode = null;
  const signals = [];
  child.kill = (signal) => { signals.push(signal); if (signal === 'SIGKILL') { child.exitCode = null; child.signalCode = signal; child.emit('exit', null, signal); } return true; };
  await stopTunnel(child, 5);
  assert.deepEqual(signals, ['SIGTERM', 'SIGKILL']);
  assert.equal(typeof startTunnel, 'function');
});

test('server security check rejects public source patterns', async () => {
  const source = await readFile(new URL('../src/server.ts', import.meta.url), 'utf8');
  assert.match(source, /trustedOrigin/);
  assert.match(source, /content-security-policy/);
  assert.match(source, /x-frame-options/);
});

test('tunnel controller reports lifecycle without exposing secrets', async () => {
  const listeners = new Map();
  const fake = {
    pid: 123, exitCode: null, signalCode: null,
    once(event, fn) { const list = listeners.get(event) ?? []; list.push(fn); listeners.set(event, list); },
    kill(signal) { this.signalCode = signal === 'SIGKILL' ? 'SIGKILL' : 'SIGTERM'; for (const fn of listeners.get('exit') ?? []) fn(null, signal); },
  };
  const controller = new TunnelController({ target: 'owner@host', localPort: 17432 }, () => fake);
  assert.deepEqual(controller.status(), { state: 'stopped' });
  assert.deepEqual(await controller.start(), { state: 'starting', pid: 123 });
  for (const fn of listeners.get('spawn') ?? []) fn();
  assert.deepEqual(controller.status(), { state: 'running', pid: 123 });
  assert.deepEqual(await controller.stop(), { state: 'stopped' });
  assert.equal(JSON.stringify(controller.status()).includes('owner@host'), false);
});

test('tunnel controller serializes start behind an in-flight stop', async () => {
  const children = [];
  const spawn = () => {
    const listeners = new Map();
    const child = {
      pid: 321 + children.length, exitCode: null, signalCode: null,
      once(event, fn) { const list = listeners.get(event) ?? []; list.push(fn); listeners.set(event, list); },
      kill(signal) {
        if (children.length > 1 || signal === 'SIGKILL') {
          this.exitCode = signal === 'SIGKILL' ? null : 0;
          this.signalCode = signal === 'SIGKILL' ? 'SIGKILL' : 'SIGTERM';
          this.emit('exit', this.exitCode, this.signalCode);
        }
        return true;
      },
      emit(event, ...args) { for (const fn of listeners.get(event) ?? []) fn(...args); },
    };
    children.push(child);
    return child;
  };
  const controller = new TunnelController({ target: 'owner@host', localPort: 17432 }, spawn);
  await controller.start();
  const first = children[0];
  const stopping = controller.stop();
  const starting = controller.start();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(controller.status().state, 'stopping');
  first.emit('spawn');
  assert.equal(controller.status().state, 'stopping', 'late spawn must not resurrect a stopping child');
  first.exitCode = 0;
  first.emit('exit', 0, null);
  assert.deepEqual(await stopping, { state: 'stopped' });
  assert.equal((await starting).state, 'starting');
  const second = children[1];
  second.emit('spawn');
  assert.equal(controller.status().state, 'running');
  await controller.stop();
});

test('browser transfer controls enforce decoded progress, file bounds, and abort cleanup', () => {
  const source = readFile(new URL('../public/app.js', import.meta.url), 'utf8');
  return source.then((text) => {
    assert.match(text, /file\.size > MAX_TRANSFER_BYTES/);
    assert.match(text, /offset \+= part\.length/);
    assert.doesNotMatch(text, /Math\.floor\(atob\(chunk\.data_base64\)\.length \* 3 \/ 4\)/);
    assert.match(text, /Abort upload/);
    assert.match(text, /method: 'DELETE'/);
  });
});

test('no raw credential is present in the public console source', async () => {
  const source = await readFile(new URL('../public/app.js', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /localStorage|sessionStorage|Authorization|owner_key/);
});
