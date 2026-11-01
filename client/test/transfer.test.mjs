import { test } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { Gateway, GatewayError } from '../dist/index.js';

// The transfer phase moves bytes and commits nothing, while still using
// POST and DELETE. Classifying it as a write told the operator that "a
// write may have committed" about a request that had neither a write nor
// an idempotency key -- advice nobody can follow. The CLI and the Rust
// client carry the same third state, and these tests pin that the
// TypeScript client now does too.

function mockGateway(handler) {
  const server = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => handler(req, res));
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({
      server,
      address: `http://127.0.0.1:${server.address().port}`,
    }));
  });
}

const PROD_READ_MS = 10_000;
const PROD_WRITE_MS = 60_000;

test('a severed export is an interrupted transfer, not a write that may have committed', async () => {
  const mock = await mockGateway((req) => req.socket.destroy());
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      () => db.exportSnapshot(),
      (error) => {
        assert.ok(error instanceof GatewayError);
        assert.equal(error.code, 'transport_unavailable',
          `a severed export must not report ${error.code}`);
        assert.match(error.message, /transfer/i,
          'the message must name the transfer, so the operator retries the transfer');
        return true;
      },
    );
  } finally { mock.server.close(); }
});

test('a severed import begin is an interrupted transfer too', async () => {
  const mock = await mockGateway((req) => req.socket.destroy());
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      () => db.importChunked('game_a', new Uint8Array([1, 2, 3])),
      (error) => {
        assert.equal(error.code, 'transport_unavailable',
          `a severed import begin must not report ${error.code}`);
        return true;
      },
    );
  } finally { mock.server.close(); }
});

test('a transfer gets the long budget, because it moves bytes', async () => {
  // A transfer is not a read: it moves up to 128 MiB, so it waits like a
  // write does. It just does not get the write's remedy.
  const original = AbortSignal.timeout.bind(AbortSignal);
  const delays = [];
  AbortSignal.timeout = (ms) => { delays.push(ms); return original(ms); };
  try {
    const mock = await mockGateway((req) => req.socket.destroy());
    try {
      const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
      await assert.rejects(() => db.exportSnapshot(), () => true);
    } finally { mock.server.close(); }
    assert.deepEqual(delays, [PROD_WRITE_MS],
      'a transfer must wait the write budget, not the read budget');
  } finally { AbortSignal.timeout = original; }
});

test('a literal loopback origin is accepted, whatever address it carries', () => {
  // The CLI and the Rust client accept any literal loopback IP, so a
  // caller that follows the CLI's documented behaviour is not refused by
  // the TypeScript client for using 127.0.0.2.
  for (const origin of ['http://127.0.0.1:7432/', 'http://127.0.0.2:7432/', 'http://[::1]:7432/']) {
    assert.doesNotThrow(() => new Gateway(origin, 'a'.repeat(64)),
      `${origin} is a loopback origin and must be accepted`);
  }
  for (const origin of ['http://10.0.0.1:7432/', 'http://localhost:7432/', 'http://example.com/']) {
    assert.throws(() => new Gateway(origin, 'a'.repeat(64)),
      `${origin} is not a literal loopback origin and must be refused`);
  }
});
