// Shadow mode against a real dg: one database stands in for the primary
// (adapted to the @libsql/client shape), another is the shadow.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Gateway } from '../dist/index.js';
import { withShadow, compareDatabases, toDgStatement } from '../dist/shadow.js';

const binary = join(import.meta.dirname, '../../.cargo/target/debug/dg');

async function withGateway(run) {
  const dataDir = mkdtempSync(join(tmpdir(), 'dg-shadow-'));
  const init = spawn(binary, ['init', '--data-dir', dataDir]);
  const owner = JSON.parse(await new Promise((resolve, reject) => {
    let out = '';
    init.stdout.on('data', (c) => (out += c));
    init.on('exit', (code) => (code === 0 ? resolve(out) : reject(new Error(out))));
  })).owner_key;
  const child = spawn(binary, ['serve', '--listen', '127.0.0.1:0', '--data-dir', dataDir], { stdio: ['ignore', 'ignore', 'pipe'] });
  const address = await new Promise((resolve, reject) => {
    let buffer = '';
    child.stderr.on('data', (chunk) => {
      buffer += chunk;
      for (const line of buffer.split('\n')) {
        try { const log = JSON.parse(line); if (log.fields?.address) resolve(log.fields.address); } catch { /* partial */ }
      }
    });
    child.on('exit', (code) => reject(new Error(`server exited: ${code}`)));
  });
  try {
    const gateway = new Gateway(`http://${address}`, owner);
    for (const name of ['prim', 'shad']) await gateway.request('/v1/databases', 'POST', { name });
    await run(gateway.database('prim'), gateway.database('shad'));
  } finally {
    child.kill('SIGTERM');
    await new Promise((resolve) => child.on('exit', resolve));
    rmSync(dataDir, { recursive: true, force: true });
  }
}

/** A dg database dressed as a @libsql/client Client. */
function asPrimary(db) {
  const shape = (r) => ({ columns: r.columns, rows: r.rows, rowsAffected: r.rows_affected });
  const statement = (s) => {
    const converted = toDgStatement(s);
    if (typeof converted === 'string') throw new Error(converted);
    return converted;
  };
  return {
    async execute(s) { const st = statement(s); return shape(await db.execute(st.sql, st.args)); },
    async batch(ss) { return (await db.batch(ss.map(statement))).results.map(shape); },
  };
}

test('writes are mirrored in order and the copies compare identical, across pages', async () => {
  await withGateway(async (prim, shad) => {
    const divergences = [];
    const db = withShadow(asPrimary(prim), shad, { onDivergence: (d) => divergences.push(d) });
    await db.execute('CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, avatar BLOB, score REAL)');
    await db.execute({ sql: 'INSERT INTO users (name, avatar, score) VALUES (?, ?, ?)', args: ['aria', new Uint8Array([1, 2, 255]), 1.5] });
    await db.execute({ sql: 'INSERT INTO users (name, score) VALUES (?, ?)', args: ['bigint id', 42n] });
    await db.batch([
      { sql: 'UPDATE users SET score = score + 1 WHERE name = ?', args: ['aria'] },
      'ALTER TABLE users ADD COLUMN email TEXT',
      { sql: 'INSERT INTO users (name, email) VALUES (?, ?)', args: ['nova', 'n@x'] },
    ], 'write');
    await db.execute('CREATE TABLE big (n INTEGER PRIMARY KEY, label TEXT)');
    await db.execute("WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 1234) INSERT INTO big SELECT i, 'row ' || i FROM s");
    // Reads go to the primary only (sampleReads defaults to 0).
    const read = await db.execute('SELECT count(*) FROM big');
    assert.deepEqual(read.rows, [[1234]]);
    await db.flush();
    assert.deepEqual(divergences, []);
    assert.equal(db.stats.mirroredWrites, 8);
    const report = await compareDatabases(asPrimary(prim), shad);
    assert.equal(report.identical, true, JSON.stringify(report));
    assert.deepEqual(report.tables.map((t) => [t.table, t.primaryRows, t.shadowRows]), [['big', 1234, 1234], ['users', 3, 3]]);
  });
});

test('a write dg refuses is reported, and the application call still succeeds', async () => {
  await withGateway(async (prim, shad) => {
    const divergences = [];
    const db = withShadow(asPrimary(prim), shad, { onDivergence: (d) => divergences.push(d) });
    // A table the shadow was never given: the copies already differ.
    await prim.execute('CREATE TABLE only_primary (x)');
    const result = await db.execute({ sql: 'INSERT INTO only_primary VALUES (?)', args: [1] });
    assert.equal(result.rowsAffected, 1);
    await db.flush();
    assert.equal(divergences.length, 1);
    assert.equal(divergences[0].kind, 'shadow_error');
    const report = await compareDatabases(asPrimary(prim), shad);
    assert.equal(report.identical, false);
    assert.deepEqual(report.schemaDifferences, ['only_primary: only on the primary']);
  });
});

test('a failed primary write is not mirrored, and the error reaches the application', async () => {
  await withGateway(async (prim, shad) => {
    const db = withShadow(asPrimary(prim), shad, { onDivergence: () => assert.fail('no divergence expected') });
    await db.execute('CREATE TABLE t (x UNIQUE)');
    await db.execute({ sql: 'INSERT INTO t VALUES (?)', args: [1] });
    await assert.rejects(db.execute({ sql: 'INSERT INTO t VALUES (?)', args: [1] }));
    await db.flush();
    assert.equal(db.stats.mirroredWrites, 2);
    assert.equal((await compareDatabases(asPrimary(prim), shad)).identical, true);
  });
});

test('sampled reads and the comparison both catch drift', async () => {
  await withGateway(async (prim, shad) => {
    const divergences = [];
    const db = withShadow(asPrimary(prim), shad, { sampleReads: 1, onDivergence: (d) => divergences.push(d) });
    await db.execute('CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)');
    await db.execute({ sql: 'INSERT INTO t (v) VALUES (?), (?)', args: ['a', 'b'] });
    await db.flush();
    await db.execute('SELECT * FROM t ORDER BY id');
    await db.flush();
    assert.deepEqual(divergences, []);
    assert.equal(db.stats.comparedReads, 1);
    // Something changes the shadow behind the primary's back.
    await shad.execute("UPDATE t SET v = 'tampered' WHERE id = 2");
    await db.execute('SELECT * FROM t ORDER BY id');
    await db.flush();
    assert.deepEqual(divergences.map((d) => d.kind), ['read_mismatch']);
    const report = await compareDatabases(asPrimary(prim), shad);
    assert.equal(report.identical, false);
    assert.deepEqual(report.tables[0].firstDifferences, [[[2, 'b'], [2, 'tampered']]]);
  });
});

test('statements dg cannot mirror exactly are reported, not sent', async () => {
  await withGateway(async (_prim, shad) => {
    const divergences = [];
    // A stub primary: it accepts anything, as libsql would.
    const stub = {
      async execute() { return { columns: [], rows: [], rowsAffected: 1 }; },
      async batch(ss) { return ss.map(() => ({ columns: [], rows: [], rowsAffected: 1 })); },
    };
    const db = withShadow(stub, shad, { onDivergence: (d) => divergences.push(d) });
    await shad.execute('CREATE TABLE t (a, b)');
    await db.execute({ sql: 'INSERT INTO t VALUES (:a, :b)', args: { a: 1, b: 2 } });
    await db.execute({ sql: 'INSERT INTO t VALUES (?, ?)', args: [2n ** 60n, 1] });
    await db.execute("INSERT INTO t VALUES (random(), datetime('now'))");
    await db.flush();
    assert.deepEqual(divergences.map((d) => d.kind), ['unsupported', 'unsupported', 'nondeterministic']);
    // The nondeterministic one is still mirrored; the unsupported ones are not.
    assert.deepEqual((await shad.execute('SELECT count(*) FROM t')).rows, [[1]]);
  });
});
