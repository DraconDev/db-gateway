import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import crypto from 'node:crypto';
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
    await run(new Gateway(`http://${address}`, ownerKey), address, dataDir, ownerKey);
  } finally {
    child.kill('SIGTERM');
    await new Promise((resolve) => child.on('exit', resolve));
    rmSync(dataDir, { recursive: true, force: true });
  }
}

test('execute, atomic batch, idempotent replay, and denial paths', async () => {
  await withServer(async (gateway, address, dataDir, ownerKey) => {
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
    // JavaScript callers can bypass TS types: malformed values fail locally
    // before transport rather than relying on the server's matching guard.
    await assert.rejects(db.execute('SELECT ?', [{ integer: '42' }]), /Invalid SQL argument/);
    await assert.rejects(db.batch([{ sql: 'SELECT ?', args: [{ integer: '42' }] }]), /Invalid SQL argument/);
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

    // M2 lifecycle: clone via the API and verify an independent, complete copy.
    await db.execute('INSERT INTO players (name) VALUES (?)', ['pre-clone']);
    await gateway.request('/v1/databases/game_a/clone', 'POST', { destination: 'game_a_copy' });
    const copyDb = new Gateway(`http://${address}`, ownerKey).database('game_a_copy');
    const cloned = await copyDb.execute('SELECT count(*) FROM players');
    const original = await db.execute('SELECT count(*) FROM players');
    assert.equal(cloned.rows[0][0], original.rows[0][0], 'clone must carry all committed rows');
    // Writes to the original after cloning do not affect the copy.
    await db.execute('INSERT INTO players (name) VALUES (?)', ['post-clone']);
    const postClone = await db.execute('SELECT count(*) FROM players');
    const copyAfter = await copyDb.execute('SELECT count(*) FROM players');
    assert.equal(postClone.rows[0][0], original.rows[0][0] + 1);
    assert.equal(copyAfter.rows[0][0], cloned.rows[0][0], 'clone must be independent');

    // Export via the API: base64 snapshot decodes to a valid SQLite file.
    const exported = await gateway.request('/v1/databases/game_a/export', 'POST', {});
    const snapshot = Buffer.from(exported.data_base64, 'base64');
    assert.equal(exported.size, snapshot.length);
    assert.equal(exported.sha256, crypto.createHash('sha256').update(snapshot).digest('hex'));
    writeFileSync(join(dataDir, 'api-export.sqlite'), snapshot);
    // Import it back as a new database through the API.
    await gateway.request('/v1/databases/game_a/import', 'POST', {
      name: 'game_a_restored',
      data_base64: exported.data_base64,
    });
    const restored = await new Gateway(`http://${address}`, ownerKey).database('game_a_restored')
      .execute('SELECT count(*) FROM players');
    // The export happened after 'post-clone', so the restored copy holds the
    // full current row count.
    assert.equal(restored.rows[0][0], postClone.rows[0][0]);
  });
});

test('documented CLI walkthrough runs verbatim after bootstrap and serve', async () => {
  await withServer(async (gateway, address, dataDir) => {
    const doc = readFileSync(new URL('../../docs/api.md', import.meta.url), 'utf8');
    const block = doc.split('## CLI\n')[1].match(/```sh\n([\s\S]*?)```/)[1];
    // withServer already performs init/serve; run every remaining documented command.
    const commands = block.split('\n').filter(line => !/^dg (init|serve) /.test(line)).join('\n');
    const script = 'set -eu\ndg() { "$DG_BIN" --server "$DG_SERVER" "$@"; }\n' + commands;
    const result = spawnSync('bash', ['-c', script], {
      cwd: dataDir, encoding: 'utf8', timeout: 15_000,
      env: { ...process.env, DG_BIN: join(import.meta.dirname, '../../target/debug/dg'), DG_SERVER: `http://${address}` },
    });
    assert.equal(result.status, 0, result.stderr);
    const output = result.stdout.trim().split('\n').map(line => JSON.parse(line));
    const issued = output.find(value => typeof value.key === 'string');
    assert.deepEqual(Object.keys(issued), ['key'], 'key response contains no metadata');
    assert.match(issued.key, /^[a-f0-9]{64}$/);
    assert.deepEqual(output.at(-1), { revoked: true });
    assert.deepEqual((await gateway.database('game_a').execute('SELECT name FROM players')).rows, [['aria']]);
    assert.deepEqual((await gateway.database('game_a').execute('SELECT id FROM matches')).rows, [[7]]);
    await assert.rejects(new Gateway(`http://${address}`, issued.key).database('game_a').execute('SELECT 1'),
      (error) => error.code === 'invalid_credential');
  });
});
