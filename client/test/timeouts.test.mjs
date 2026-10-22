import { test } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { Gateway, GatewayError, isReadOnly, semanticsOf, semanticsOfStatements } from '../dist/index.js';

// A local mock endpoint that answers after a fixed delay. The delays here are
// short (tens of milliseconds) because the client's own timeouts are tens of
// SECONDS; what is under test is which budget a request is put on, not the
// absolute wall time. The budgets themselves are asserted separately below so
// a change to either constant fails a test instead of silently weakening the
// contract.
function slowEndpoint(delayMs, payload = { columns: [], rows: [], rows_affected: 1 }) {
  const server = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      setTimeout(() => {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(payload));
      }, delayMs);
    });
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({
      server,
      address: `http://127.0.0.1:${server.address().port}`,
    }));
  });
}

// Record the delay the client asks of AbortSignal.timeout, so the budget is
// an observed number rather than something inferred from waiting a minute.
function spyOnTimeoutDelays() {
  const original = AbortSignal.timeout.bind(AbortSignal);
  const delays = [];
  AbortSignal.timeout = (ms) => {
    delays.push(ms);
    return original(ms);
  };
  return { delays, restore: () => { AbortSignal.timeout = original; } };
}

test('a read-only statement is classified as a read; a writing statement is not', () => {
  const reads = [
    'SELECT 1',
    'select 1',
    '  SELECT id FROM notes',
    'VALUES (1)',
    'EXPLAIN QUERY PLAN SELECT 1',
    'WITH recent AS (SELECT id FROM notes) SELECT * FROM recent',
  ];
  const writes = [
    'INSERT INTO notes VALUES (1)',
    'UPDATE notes SET body = ?',
    'DELETE FROM notes',
    'CREATE TABLE notes (id INTEGER)',
    'DROP TABLE notes',
    'WITH moved AS (SELECT id FROM notes) INSERT INTO notes SELECT id FROM moved',
    'ALTER TABLE notes ADD COLUMN body TEXT',
    'REPLACE INTO notes VALUES (1)',
    'VACUUM',
    'BEGIN',
    'PRAGMA journal_mode',
  ];
  for (const sql of reads) {
    assert.equal(isReadOnly(sql), true, `${JSON.stringify(sql)} should be a read`);
    assert.equal(semanticsOf(sql), 'read', `${JSON.stringify(sql)} should have read semantics`);
  }
  for (const sql of writes) {
    assert.equal(isReadOnly(sql), false, `${JSON.stringify(sql)} should be a write`);
    assert.equal(semanticsOf(sql), 'write', `${JSON.stringify(sql)} should have write semantics`);
  }
  // Fail closed: anything ambiguous is a write.
  assert.equal(isReadOnly(''), false);
  assert.equal(isReadOnly('nonsense'), false);
});

test('a batch is a read only when every statement in it is one', () => {
  assert.equal(semanticsOfStatements([{ sql: 'SELECT 1' }, { sql: 'SELECT 2' }]), 'read');
  assert.equal(semanticsOfStatements([{ sql: 'SELECT 1' }, { sql: 'INSERT INTO t VALUES (1)' }]), 'write');
  assert.equal(semanticsOfStatements([{ sql: 'INSERT INTO t VALUES (1)' }]), 'write');
  // An empty batch cannot commit anything.
  assert.equal(semanticsOfStatements([]), 'read');
});

test('a fast write and a fast read both succeed', async () => {
  const mock = await slowEndpoint(5);
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    const written = await db.execute('INSERT INTO t VALUES (?)', [1]);
    assert.equal(written.rows_affected, 1);
    const read = await db.execute('SELECT * FROM t', []);
    assert.deepEqual(read.rows, []);
  } finally {
    mock.server.close();
  }
});

test('a read-only query that fails in transport is never reported as a write that may have committed', async () => {
  const server = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => req.socket.destroy());
  });
  const mock = await new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({ server, address: `http://127.0.0.1:${server.address().port}` }));
  });
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    // A SELECT is a POST, but it cannot have committed anything: the failure
    // must say the gateway is unavailable, not that a write may have landed.
    await assert.rejects(
      () => db.execute('SELECT * FROM t', []),
      (error) => {
        assert.ok(error instanceof GatewayError);
        assert.equal(error.code, 'transport_unavailable', `a read must not report ${error.code}`);
        return true;
      },
    );
    // The same transport failure on a write IS in doubt, and says so.
    await assert.rejects(
      () => db.execute('INSERT INTO t VALUES (?)', [1]),
      (error) => {
        assert.ok(error instanceof GatewayError);
        assert.equal(error.code, 'outcome_unknown');
        return true;
      },
    );
  } finally {
    mock.server.close();
  }
});

test('a read-only batch that fails in transport is reported as unavailable, not as a write in doubt', async () => {
  const server = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => req.socket.destroy());
  });
  const mock = await new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({ server, address: `http://127.0.0.1:${server.address().port}` }));
  });
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      () => db.batch([{ sql: 'SELECT 1' }, { sql: 'SELECT 2' }]),
      (error) => {
        assert.equal(error.code, 'transport_unavailable', `a read batch must not report ${error.code}`);
        return true;
      },
    );
    await assert.rejects(
      () => db.batch([{ sql: 'SELECT 1' }, { sql: 'INSERT INTO t VALUES (1)' }]),
      (error) => {
        assert.equal(error.code, 'outcome_unknown', 'a batch with a write in it IS in doubt');
        return true;
      },
    );
  } finally {
    mock.server.close();
  }
});

// The budgets themselves. These are the regression pin: on 2026-10-22 commit
// c97eb2b deleted the write budget and silently reverted every request to ten
// seconds, and nothing failed. A write must outlive a read by a wide margin.
test('a read is given ten seconds and a write is given a minute', async () => {
  const db = new Gateway('http://127.0.0.1:1', 'a'.repeat(64)).database('game_a');
  const spy = spyOnTimeoutDelays();
  try {
    const read = db.execute('SELECT 1', []);
    const write = db.execute('INSERT INTO t VALUES (?)', [1]);
    const readBatch = db.batch([{ sql: 'SELECT 1' }]);
    const writeBatch = db.batch([{ sql: 'INSERT INTO t VALUES (1)' }]);
    // A management route has no statements to classify, so the method is the
    // answer: GET reads, everything else is a write.
    const list = new Gateway('http://127.0.0.1:1', 'a'.repeat(64)).listDatabases();
    await Promise.allSettled([read, write, readBatch, writeBatch, list]);

    assert.deepEqual(
      spy.delays,
      [10_000, 60_000, 10_000, 60_000, 10_000],
      'a read gets 10s, a write gets 60s -- one budget for both was the regression',
    );
  } finally {
    spy.restore();
  }
});
