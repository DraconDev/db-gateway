import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EventEmitter } from 'node:events';
import { ApiError, GatewayApi } from '../dist/api.js';
import { configFromEnv, readCredential } from '../dist/server.js';
import { startTunnel, stopTunnel, tunnelCommand, TunnelController } from '../dist/tunnel.js';

const key = 'a'.repeat(64);

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

test('tunnel command is loopback-only and lifecycle is injectable', async () => {
  const spec = tunnelCommand({ target: 'owner@host', localPort: 17432, remotePort: 7432, identityFile: '/tmp/id' });
  assert.deepEqual(spec.args.slice(0, 5), ['-N', '-T', '-o', 'ExitOnForwardFailure=yes', '-L']);
  assert.match(spec.args[5], /^127\.0\.0\.1:17432:127\.0\.0\.1:7432$/);
  const child = new EventEmitter();
  child.pid = 123; child.exitCode = null; child.signalCode = null;
  child.kill = (signal) => { child.signalCode = signal; child.emit('exit', null, signal); return true; };
  await stopTunnel(child);
  assert.equal(child.signalCode, 'SIGTERM');
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

test('no raw credential is present in the public console source', async () => {
  const source = await readFile(new URL('../public/app.js', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /localStorage|sessionStorage|Authorization|owner_key/);
});
