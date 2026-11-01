import { test } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { Gateway, GatewayError } from '../dist/index.js';

// Local mock endpoint: counts received requests, then kills the connection
// (or sends garbage) so the acknowledgement is lost. Never touches a real
// gateway or disk.
function mockEndpoint(respond) {
  let received = 0;
  const server = http.createServer((req, res) => {
    received += 1;
    req.resume();
    req.on('end', () => respond(req, res));
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({
      server,
      address: `127.0.0.1:${server.address().port}`,
      count: () => received,
    }));
  });
}

test('severed response surfaces outcome_unknown and never resends the write', async () => {
  const mock = await mockEndpoint((req, res) => req.socket.destroy());
  try {
    const db = new Gateway(`http://${mock.address}`, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      db.execute('INSERT INTO t VALUES (?)', [1]),
      (error) => error instanceof GatewayError && error.code === 'outcome_unknown',
    );
    assert.equal(mock.count(), 1);
  } finally {
    mock.server.close();
  }
});

test('truncated body surfaces outcome_unknown and never resends the write', async () => {
  const mock = await mockEndpoint((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json', Connection: 'close' });
    res.write('{"columns":');
    res.end();
  });
  try {
    const db = new Gateway(`http://${mock.address}`, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      db.execute('INSERT INTO t VALUES (?)', [1]),
      (error) => error instanceof GatewayError && error.code === 'outcome_unknown',
    );
    assert.equal(mock.count(), 1);
  } finally {
    mock.server.close();
  }
});

test('explicit replay requires the original idempotency key', async () => {
  const mock = await mockEndpoint((req, res) => req.socket.destroy());
  try {
    const db = new Gateway(`http://${mock.address}`, 'a'.repeat(64)).database('game_a');
    assert.throws(
      () => db.retry('INSERT INTO t VALUES (?)', [1], ''),
      /Replay requires/,
    );
    assert.equal(mock.count(), 0);
  } finally {
    mock.server.close();
  }
});

// A read never commits anything, so it must not be reported with the code
// that tells an operator a write may have committed. The client classifies
// by the statements it sends, with the same rule the server uses.
test('severed read surfaces transport_unavailable, never outcome_unknown', async () => {
  const mock = await mockEndpoint((req, res) => req.socket.destroy());
  try {
    const gateway = new Gateway(`http://${mock.address}`, 'a'.repeat(64));
    await assert.rejects(
      gateway.listDatabases(),
      (error) => error instanceof GatewayError && error.code === 'transport_unavailable',
    );
    assert.equal(mock.count(), 1);
  } finally {
    mock.server.close();
  }
});

test('truncated read surfaces invalid_gateway_response, never outcome_unknown', async () => {
  const mock = await mockEndpoint((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json', Connection: 'close' });
    res.write('{"databases":[');
    res.end();
  });
  try {
    const gateway = new Gateway(`http://${mock.address}`, 'a'.repeat(64));
    await assert.rejects(
      gateway.listDatabases(),
      (error) => error instanceof GatewayError && error.code === 'invalid_gateway_response',
    );
    assert.equal(mock.count(), 1);
  } finally {
    mock.server.close();
  }
});

test('oversized read response surfaces invalid_gateway_response, never outcome_unknown', async () => {
  const mock = await mockEndpoint((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ databases: [], padding: 'x'.repeat(4096) }));
  });
  try {
    const gateway = new Gateway(`http://${mock.address}`, 'a'.repeat(64));
    await assert.rejects(
      gateway.request('/v1/databases', 'GET', undefined, { maxResponseBytes: 64 }),
      (error) => error instanceof GatewayError && error.code === 'invalid_gateway_response',
    );
    assert.equal(mock.count(), 1);
  } finally {
    mock.server.close();
  }
});

test('malformed read error envelope surfaces invalid_gateway_response, never outcome_unknown', async () => {
  const mock = await mockEndpoint((req, res) => {
    res.writeHead(500, { 'Content-Type': 'application/json' });
    res.end('{"error":"not-an-object"}');
  });
  try {
    const gateway = new Gateway(`http://${mock.address}`, 'a'.repeat(64));
    await assert.rejects(
      gateway.listDatabases(),
      (error) => error instanceof GatewayError && error.code === 'invalid_gateway_response',
    );
    assert.equal(mock.count(), 1);
  } finally {
    mock.server.close();
  }
});
