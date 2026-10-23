import { test } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { Gateway, GatewayError, isReadOnly, semanticsOf, semanticsOfStatements } from '../dist/index.js';

// These tests prove REAL delayed behaviour, not that a constant holds a
// number: the gateway is mocked to answer after a chosen delay, and the client
// is given budgets scaled down to milliseconds. What is asserted is which
// outcome each request reaches, so a change to the classification OR to the
// budget wiring fails here.
//
// Production budgets are asserted separately (see 'the published budgets'),
// because the injected budgets are the test's own choice and prove nothing
// about what a real caller gets.
const PROD_READ_MS = 10_000;
const PROD_WRITE_MS = 60_000;

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

const OK_RESULT = { columns: ['n'], rows: [[1]], rows_affected: 1 };

/// Answer successfully, but only after `delayMs`.
function answerAfter(delayMs, payload = OK_RESULT) {
  return (_req, res) => {
    setTimeout(() => {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(payload));
    }, delayMs);
  };
}

/// Send the status line and headers at once, then stall the body for
/// `bodyDelayMs`. This is the case where the response is NOT malformed and
/// nothing has failed yet -- only the clock runs out mid-body.
function headersThenStall(bodyDelayMs, body = JSON.stringify(OK_RESULT)) {
  return (_req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.write(body.slice(0, 4));
    setTimeout(() => { try { res.end(body.slice(4)); } catch { /* client gone */ } }, bodyDelayMs);
  };
}

function closeAll(mock) { mock.server.close(); }

// ---------------------------------------------------------------- classifier

test('a keyword inside a literal or comment does not make a read a write', () => {
  // SQLite itself agrees these write nothing: each returns rows and leaves
  // total_changes at 0. Classifying them as writes reported a read-only query
  // as outcome_unknown and made the caller wait a minute instead of ten
  // seconds. Every case below was run through sqlite3 to confirm it is a read.
  const reads = [
    "SELECT 'DELETE'",
    "SELECT 'INSERT INTO t VALUES (1)'",
    'SELECT 1 -- DELETE',
    'SELECT 1 /* INSERT INTO t VALUES (1) */',
    'SELECT 1 -- DROP TABLE t',
    ['SELECT 1', '/* UPDATE t SET x = 1 */'].join('\n'),
    'SELECT "DELETE" FROM t',
    'SELECT `DROP` FROM t',
    'SELECT [INSERT] FROM t',
    "SELECT 'it''s a DELETE'",
    "WITH x AS (SELECT 'DROP TABLE t') SELECT * FROM x",
    "WITH updated AS (SELECT 1) SELECT * FROM updated",
  ];
  for (const sql of reads) {
    assert.equal(isReadOnly(sql), true, `${JSON.stringify(sql)} must be a read`);
    assert.equal(semanticsOf(sql), 'read', `${JSON.stringify(sql)} must have read semantics`);
  }
});

test('a real modifying keyword is still a write, including inside a WITH', () => {
  const writes = [
    'INSERT INTO t VALUES (1)',
    'UPDATE t SET x = 1',
    'DELETE FROM t',
    'CREATE TABLE t (x)',
    'DROP TABLE t',
    'ALTER TABLE t ADD COLUMN y',
    'REPLACE INTO t VALUES (1)',
    'VACUUM',
    'REINDEX t',
    'WITH m AS (SELECT 1) INSERT INTO t SELECT * FROM m',
    "WITH m AS (SELECT 1) UPDATE t SET x = 'DELETE'",
    // A literal before the real keyword: the literal is skipped, the keyword
    // after it is not.
    "WITH m AS (SELECT 'none') DELETE FROM t",
  ];
  for (const sql of writes) {
    assert.equal(isReadOnly(sql), false, `${JSON.stringify(sql)} must be a write`);
    assert.equal(semanticsOf(sql), 'write', `${JSON.stringify(sql)} must have write semantics`);
  }
  // Fail closed on anything unrecognisable.
  assert.equal(isReadOnly(''), false);
  assert.equal(isReadOnly('nonsense'), false);
  assert.equal(isReadOnly('BEGIN'), false);
  assert.equal(isReadOnly('PRAGMA journal_mode'), false);
});

test('a batch is a read only when every statement in it is one', () => {
  assert.equal(semanticsOfStatements([{ sql: 'SELECT 1' }, { sql: "SELECT 'DELETE'" }]), 'read');
  assert.equal(semanticsOfStatements([{ sql: 'SELECT 1' }, { sql: 'INSERT INTO t VALUES (1)' }]), 'write');
  assert.equal(semanticsOfStatements([]), 'read');
});

// -------------------------------------------------- delayed completion (real)

test('a write that answers after the READ budget still completes', async () => {
  // The response takes three times the read budget. A write must wait.
  const mock = await mockGateway(answerAfter(300));
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64), { readTimeoutMs: 100, writeTimeoutMs: 2_000 }).database('game_a');
    const result = await db.execute('INSERT INTO t VALUES (?)', [1]);
    assert.deepEqual(result.rows, [[1]], 'the write should have completed, not timed out');
  } finally { closeAll(mock); }
});

test('a read-only query that answers after the read budget fails as transport_unavailable', async () => {
  const mock = await mockGateway(answerAfter(300));
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64), { readTimeoutMs: 100, writeTimeoutMs: 2_000 }).database('game_a');
    await assert.rejects(
      () => db.execute('SELECT 1', []),
      (error) => {
        assert.ok(error instanceof GatewayError);
        assert.equal(error.code, 'transport_unavailable', `a timed-out read must not report ${error.code}`);
        return true;
      },
    );
  } finally { closeAll(mock); }
});

test('a read-only query whose LITERAL says DELETE times out as a read, not as a write in doubt', async () => {
  // The end-to-end consequence of the classifier fix: this statement is a
  // read, so it gets the read budget and the read failure code.
  const mock = await mockGateway(answerAfter(300));
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64), { readTimeoutMs: 100, writeTimeoutMs: 2_000 }).database('game_a');
    await assert.rejects(
      () => db.execute("SELECT 'DELETE'", []),
      (error) => {
        assert.equal(error.code, 'transport_unavailable', `SELECT 'DELETE' is a read and must not report ${error.code}`);
        return true;
      },
    );
  } finally { closeAll(mock); }
});

test('a read whose headers arrive but whose BODY stalls past the budget is transport_unavailable', async () => {
  // The auditor's case: the response is not malformed and nothing failed --
  // the clock simply ran out mid-body. Reporting that as
  // invalid_gateway_response told the operator the gateway said something
  // wrong, which is a different and wrong diagnosis.
  const mock = await mockGateway(headersThenStall(400));
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64), { readTimeoutMs: 120, writeTimeoutMs: 2_000 }).database('game_a');
    await assert.rejects(
      () => db.execute('SELECT 1', []),
      (error) => {
        assert.ok(error instanceof GatewayError);
        assert.equal(error.code, 'transport_unavailable', `a stalled read body must report transport_unavailable, not ${error.code}`);
        return true;
      },
    );
  } finally { closeAll(mock); }
});

test('a genuinely truncated read is still invalid_gateway_response, not a timeout', async () => {
  // The distinction the fix must preserve: a response that ENDS badly is a
  // malformed response, and a retry is not the operator's next move.
  const mock = await mockGateway((_req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.write('{"columns":');
    setTimeout(() => { try { res.socket.destroy(); } catch { /* already gone */ } }, 20);
  });
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64), { readTimeoutMs: 2_000, writeTimeoutMs: 2_000 }).database('game_a');
    await assert.rejects(
      () => db.execute('SELECT 1', []),
      (error) => {
        assert.equal(error.code, 'invalid_gateway_response', `a truncated response is malformed, not a timeout: got ${error.code}`);
        return true;
      },
    );
  } finally { closeAll(mock); }
});

// ------------------------------------------------------- failure semantics

test('a read-only query that fails in transport is never reported as a write that may have committed', async () => {
  const mock = await mockGateway((req) => req.socket.destroy());
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      () => db.execute('SELECT * FROM t', []),
      (error) => { assert.equal(error.code, 'transport_unavailable'); return true; },
    );
    await assert.rejects(
      () => db.execute('INSERT INTO t VALUES (?)', [1]),
      (error) => { assert.equal(error.code, 'outcome_unknown'); return true; },
    );
  } finally { closeAll(mock); }
});

test('a read-only batch that fails in transport is unavailable, not in doubt', async () => {
  const mock = await mockGateway((req) => req.socket.destroy());
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    await assert.rejects(
      () => db.batch([{ sql: 'SELECT 1' }, { sql: "SELECT 'DELETE'" }]),
      (error) => { assert.equal(error.code, 'transport_unavailable', `read batch reported ${error.code}`); return true; },
    );
    await assert.rejects(
      () => db.batch([{ sql: 'SELECT 1' }, { sql: 'INSERT INTO t VALUES (1)' }]),
      (error) => { assert.equal(error.code, 'outcome_unknown'); return true; },
    );
  } finally { closeAll(mock); }
});

// -------------------------------------------------- the published budgets

test('the published budgets are ten seconds for a read and a minute for a write', async () => {
  // No injected budgets here: a default Gateway must hand exactly these to
  // AbortSignal.timeout, in this order. This is the regression pin for
  // commit c97eb2b, which deleted the write budget and quietly reverted every
  // request to ten seconds.
  const original = AbortSignal.timeout.bind(AbortSignal);
  const delays = [];
  AbortSignal.timeout = (ms) => { delays.push(ms); return original(ms); };
  try {
    const db = new Gateway('http://127.0.0.1:1', 'a'.repeat(64)).database('game_a');
    const gw = new Gateway('http://127.0.0.1:1', 'a'.repeat(64));
    await Promise.allSettled([
      db.execute('SELECT 1', []),
      db.execute('INSERT INTO t VALUES (?)', [1]),
      db.batch([{ sql: 'SELECT 1' }]),
      db.batch([{ sql: 'INSERT INTO t VALUES (1)' }]),
      gw.listDatabases(),
    ]);
    assert.deepEqual(delays, [PROD_READ_MS, PROD_WRITE_MS, PROD_READ_MS, PROD_WRITE_MS, PROD_READ_MS],
      'default budgets must be 10s read / 60s write');
  } finally { AbortSignal.timeout = original; }
});

test('a fast read and a fast write both succeed', async () => {
  const mock = await mockGateway(answerAfter(5));
  try {
    const db = new Gateway(mock.address, 'a'.repeat(64)).database('game_a');
    assert.equal((await db.execute('INSERT INTO t VALUES (?)', [1])).rows_affected, 1);
    assert.deepEqual((await db.execute('SELECT 1', [])).rows, [[1]]);
  } finally { closeAll(mock); }
});
