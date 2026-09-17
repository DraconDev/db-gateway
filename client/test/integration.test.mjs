import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Gateway, GatewayError } from '../dist/index.js';

function start(dataDir) {
  const child = spawn(join(import.meta.dirname, '../../target/debug/dg'), ['serve', '--listen', '127.0.0.1:0', '--data-dir', dataDir], { stdio: ['ignore', 'ignore', 'pipe'] });
  return new Promise((resolve, reject) => {
    let buffer = '';
    child.stderr.on('data', (chunk) => {
      buffer += chunk;
      for (const line of buffer.split('\n')) {
        try {
          const log = JSON.parse(line);
          if (log.fields?.address) resolve({ child, address: log.fields.address });
        } catch { /* partial line */ }
      }
    });
    child.on('exit', (code) => reject(new Error(`server exited early: ${code}\n${buffer}`)));
  });
}

async function withServer(run) {
  const dataDir = mkdtempSync(join(tmpdir(), 'dg-client-'));
  const { child, address } = await start(dataDir);
  try {
    const init = spawn(join(import.meta.dirname, '../../target/debug/dg'), ['init', '--data-dir', dataDir]);
    const ownerKey = JSON.parse(await new Promise((resolve, reject) => {
      let out = '';
      init.stdout.on('data', (c) => (out += c));
      init.on('exit', (code) => (code === 0 ? resolve(out) : reject(new Error(out))));
    })).owner_key;
    const keyFile = join(dataDir, 'owner.key');
    writeFileSync(keyFile, ownerKey + '\n');
    chmodSync(keyFile, 0o600);
    await run(new Gateway(`http://${address}`, ownerKey), address, dataDir);
  } finally {
    child.kill('SIGTERM');
    await new Promise((resolve) => child.on('exit', resolve));
    rmSync(dataDir, { recursive: true, force: true });
  }
}

test('execute, atomic batch, idempotent replay, and denial paths', async () => {
  await withServer(async (gateway, address) => {
    const db = gateway.database('game_a');
    // create requires POST /v1/databases — do it via raw request.
    await gateway.request('/v1/databases', 'POST', { name: 'game_a' });
    await db.execute('CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT NOT NULL)');
    const inserted = await db.execute('INSERT INTO players (name) VALUES (?)', ['aria']);
    assert.equal(inserted.rows_affected, 1);
    const selected = await db.execute('SELECT id, name FROM players');
    assert.deepEqual(selected.rows, [[1, 'aria']]);
    // The public value contract matches SQLite's JSON encoding, not integer objects.
    const values = await db.execute('SELECT ?, ?, ?, ?, ?, ?', [null, true, 42, 1.5, 'text', [0, 128, 255]]);
    assert.deepEqual(values.rows, [[null, 1, 42, 1.5, 'text', [0, 128, 255]]]);
    // JavaScript callers can bypass TS types: the server still rejects objects.
    await assert.rejects(db.execute('SELECT ?', [{ integer: '42' }]),
      (error) => error instanceof GatewayError && error.code === 'invalid_request');
    await assert.rejects(db.batch([{ sql: 'SELECT ?', args: [{ integer: '42' }] }]),
      (error) => error instanceof GatewayError && error.code === 'invalid_request');
    // Atomic batch: second statement fails, first must roll back.
    await assert.rejects(
      db.batch([
        { sql: 'INSERT INTO players (name) VALUES (?)', args: ['should-not-persist'] },
        { sql: 'INSERT INTO missing_table VALUES (1)' },
      ]),
      (error) => error instanceof GatewayError,
    );
    const after = await db.execute('SELECT count(*) FROM players');
    assert.equal(after.rows[0][0], 1);
    // Idempotent replay with same key + body returns identical stored result.
    const first = await db.execute('INSERT INTO players (name) VALUES (?)', ['kim'], { idempotencyKey: 'match-final-9182' });
    const replay = await db.execute('INSERT INTO players (name) VALUES (?)', ['kim'], { idempotencyKey: 'match-final-9182' });
    assert.deepEqual(replay, first);
    const count = await db.execute('SELECT count(*) FROM players');
    assert.equal(count.rows[0][0], 2, 'write must not execute twice');
    // Read-only scoped key: read allowed, write denied at the SQLite layer.
    const rwKey = (await gateway.request('/v1/keys', 'POST', { name: 'writer', role: 'database_read_write', databases: ['game_a'] })).key;
    const roKey = (await gateway.request('/v1/keys', 'POST', { name: 'reader', role: 'database_read_only', databases: ['game_a'] })).key;
    const writerGw = new Gateway(`http://${address}`, rwKey);
    assert.equal((await writerGw.database('game_a').execute('SELECT count(*) FROM players')).rows[0][0], 2);
    const readerGw = new Gateway(`http://${address}`, roKey);
    const reader = readerGw.database('game_a');
    await reader.execute('SELECT 1');
    await assert.rejects(reader.execute('INSERT INTO players (name) VALUES (?)', ['nope']), (error) => error.code === 'sql_forbidden');
    // Cross-database denial.
    await gateway.request('/v1/databases', 'POST', { name: 'other' });
    await assert.rejects(writerGw.database('other').execute('SELECT 1'), (error) => error.code === 'database_forbidden');
  });
});
