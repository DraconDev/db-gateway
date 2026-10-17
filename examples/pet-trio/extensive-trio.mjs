// Extensive pet-trio workout: concurrency, idempotent replay, atomic
// rollback, auth boundaries, clone independence, chunked export, SIGKILL
// restart persistence, load burst, and a real versioned backup set +
// offline verify — all against a disposable local gateway.
//
// Usage: node examples/pet-trio/extensive-trio.mjs
// Exit 0 + [VERDICT] pass only if EVERY section passes. Sections run in
// order; failures are collected and reported at the end (no fail-fast,
// so one red section does not hide the rest).
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Gateway, GatewayError } from '../../client/dist/index.js';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..');
const binary = join(root, '.cargo', 'target', 'debug', 'dg');
const backupScript = join(root, 'scripts', 'micro2-backup.sh');

const say = (k, v) => console.log(`[${k}] ${typeof v === 'string' ? v : JSON.stringify(v)}`);
const failures = [];
const section = async (name, fn) => {
  try {
    const detail = await fn();
    say(`${name}_PASS`, detail ?? 'ok');
  } catch (error) {
    failures.push(name);
    say(`${name}_FAIL`, error?.message ?? String(error));
  }
};
const assert = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const expectCode = async (promise, code) => {
  try {
    await promise;
  } catch (error) {
    if (error instanceof GatewayError && error.code === code) return code;
    throw new Error(`expected ${code}, got ${error?.code ?? error?.message}`);
  }
  throw new Error(`expected ${code}, call succeeded`);
};

function start(dataDir) {
  const child = spawn(binary, ['serve', '--listen', '127.0.0.1:0', '--data-dir', dataDir], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  return new Promise((resolve, reject) => {
    let buffer = '';
    const timer = setTimeout(() => reject(new Error('server start timeout')), 30_000);
    child.stderr.on('data', (chunk) => {
      buffer += chunk;
      for (const line of buffer.split('\n')) {
        try {
          const log = JSON.parse(line);
          if (log.fields?.address) {
            clearTimeout(timer);
            resolve({ child, address: log.fields.address });
          }
        } catch {
          /* partial line */
        }
      }
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`server exited early: ${code}\n${buffer}`));
    });
  });
}

const dataDir = mkdtempSync(join(tmpdir(), 'dg-stress-'));
const backupRoot = mkdtempSync(join(tmpdir(), 'dg-stress-backups-'));
let server = null;
let origin = null;
let owner = null;
const boot = async () => {
  const started = await start(dataDir);
  server = started.child;
  origin = `http://${started.address}`;
};

try {
  say('START_EPOCH', Math.floor(Date.now() / 1000));
  await boot();
  say('LISTEN', origin);

  const out = execFileSync(binary, ['init', '--data-dir', dataDir], { encoding: 'utf8' });
  const ownerKey = JSON.parse(out).owner_key;
  const keyFile = join(dataDir, 'owner.key');
  writeFileSync(keyFile, ownerKey + '\n');
  chmodSync(keyFile, 0o600);
  owner = new Gateway(origin, ownerKey);

  for (const db of ['stress_events', 'stress_board', 'stress_todo']) {
    await owner.request('/v1/databases', 'POST', { name: db });
  }
  const keyFor = async (name, role, dbs) =>
    (await owner.request('/v1/keys', 'POST', { name, role, databases: dbs })).key;
  const evKey = await keyFor('ev_writer', 'database_read_write', ['stress_events']);
  const boardKey = await keyFor('board_writer', 'database_read_write', ['stress_board']);
  const todoRw = await keyFor('todo_writer', 'database_read_write', ['stress_todo']);
  const todoRo = await keyFor('todo_reader', 'database_read_only', ['stress_todo']);
  const gw = (key) => new Gateway(origin, key);
  const ev = () => gw(evKey).database('stress_events');
  const board = () => gw(boardKey).database('stress_board');

  await ev().execute(
    'CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL, ts INTEGER NOT NULL)'
  );
  await board().execute('CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT NOT NULL)');
  await board().execute(
    'CREATE TABLE scores (id INTEGER PRIMARY KEY, player_id INTEGER NOT NULL, points INTEGER NOT NULL)'
  );
  await gw(todoRw).database('stress_todo').execute(
    'CREATE TABLE todos (id INTEGER PRIMARY KEY, title TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)'
  );

  await section('T1_CONCURRENT_APPENDS', async () => {
    const workers = 8;
    const perWorker = 100;
    const now = Math.floor(Date.now() / 1000);
    await Promise.all(
      Array.from({ length: workers }, (_, w) =>
        (async () => {
          const db = ev();
          for (let i = 0; i < perWorker; i++) {
            await db.execute('INSERT INTO events (kind, payload, ts) VALUES (?, ?, ?)', [
              'load',
              `w${w}-${i}`,
              now + i,
            ]);
          }
        })()
      )
    );
    const count = await ev().execute('SELECT count(*) FROM events');
    assert(count.rows[0][0] === workers * perWorker, `lost writes: ${count.rows[0][0]}`);
    const distinct = await ev().execute('SELECT count(DISTINCT payload) FROM events');
    assert(distinct.rows[0][0] === workers * perWorker, 'duplicate/missing payloads');
    return { workers, perWorker, total: count.rows[0][0] };
  });

  await section('T2_IDEMPOTENT_REPLAY', async () => {
    const db = ev();
    const before = (await db.execute('SELECT count(*) FROM events')).rows[0][0];
    const key = `stress-replay-${Date.now()}`;
    const first = await db.execute('INSERT INTO events (kind, payload, ts) VALUES (?, ?, ?)', ['r', 'once', 1], { idempotencyKey: key });
    const replay = await db.execute('INSERT INTO events (kind, payload, ts) VALUES (?, ?, ?)', ['r', 'once', 1], { idempotencyKey: key });
    assert(JSON.stringify(replay) === JSON.stringify(first), 'replay result differs');
    const after = (await db.execute('SELECT count(*) FROM events')).rows[0][0];
    assert(after === before + 1, `replay double-executed: ${before} -> ${after}`);
    return { before, after };
  });

  await section('T3_ATOMIC_ROLLBACK', async () => {
    const db = board();
    const before = (await db.execute('SELECT count(*) FROM players')).rows[0][0];
    await expectCode(
      db.batch([
        { sql: 'INSERT INTO players (name) VALUES (?)', args: ['ghost'] },
        { sql: 'INSERT INTO missing_table VALUES (1)' },
      ]),
      undefined // any GatewayError code; assert below that nothing persisted
    ).catch((e) => {
      if (!/expected/.test(e.message)) throw e; // batch rejected as required
      throw new Error('bad batch unexpectedly succeeded');
    });
    const after = (await db.execute('SELECT count(*) FROM players')).rows[0][0];
    assert(after === before, `partial batch persisted: ${before} -> ${after}`);
    return { before, after };
  });

  await section('T4_AUTH_BOUNDARIES', async () => {
    const ro = gw(todoRo).database('stress_todo');
    await ro.execute('SELECT 1');
    const denyWrite = await expectCode(
      ro.execute('INSERT INTO todos (title) VALUES (?)', ['nope']), 'sql_forbidden'
    );
    const denyCross = await expectCode(ev().execute('SELECT 1').then(() => gw(evKey).database('stress_board').execute('SELECT 1')), 'database_forbidden');
    await owner.revokeKey('todo_writer');
    const denyRevoked = await expectCode(
      gw(todoRw).database('stress_todo').execute('SELECT 1'), 'invalid_credential'
    );
    const denyUnknown = await expectCode(owner.database('no_such_db').execute('SELECT 1'), 'database_not_found');
    return { denyWrite, denyCross, denyRevoked, denyUnknown };
  });

  await section('T5_CLONE_INDEPENDENCE', async () => {
    const db = board();
    await db.execute("INSERT INTO players (name) VALUES ('clone_me')");
    await owner.database('stress_board').cloneTo('board_copy');
    const copyCount = (await owner.database('board_copy').execute('SELECT count(*) FROM players')).rows[0][0];
    await db.execute("INSERT INTO players (name) VALUES ('post_clone')");
    const origCount = (await db.execute('SELECT count(*) FROM players')).rows[0][0];
    const copyAfter = (await owner.database('board_copy').execute('SELECT count(*) FROM players')).rows[0][0];
    assert(copyAfter === copyCount, 'clone changed after original write');
    assert(origCount === copyCount + 1, 'original missing post-clone row');
    await owner.database('board_copy').execute("INSERT INTO players (name) VALUES ('copy_only')");
    const copyFinal = (await owner.database('board_copy').execute('SELECT count(*) FROM players')).rows[0][0];
    assert(copyFinal === copyCount + 1, 'copy not writable');
    return { copyCount, origCount, copyFinal };
  });

  await section('T6_CHUNKED_EXPORT_MATCH', async () => {
    const full = await owner.database('stress_events').exportSnapshot();
    const chunked = await owner.database('stress_events').exportChunked();
    assert(chunked.sha256 === full.sha256, 'chunked sha differs');
    assert(chunked.size === full.size, 'chunked size differs');
    await owner.importSnapshot('events_restored', chunked.data);
    const live = (await owner.database('stress_events').execute('SELECT count(*) FROM events')).rows[0][0];
    const back = (await owner.database('events_restored').execute('SELECT count(*) FROM events')).rows[0][0];
    assert(live === back, `restore mismatch ${live} vs ${back}`);
    return { size: full.size, sha16: full.sha256.slice(0, 16), rows: live };
  });

  await section('T7_SIGKILL_RESTART', async () => {
    const counts = {};
    for (const [db, sql] of [
      ['stress_events', 'SELECT count(*) FROM events'],
      ['stress_board', 'SELECT count(*) FROM players'],
      ['board_copy', 'SELECT count(*) FROM players'],
    ]) {
      counts[db] = (await owner.database(db).execute(sql)).rows[0][0];
    }
    server.kill('SIGKILL');
    await new Promise((resolve) => server.on('exit', resolve));
    await boot();
    owner = new Gateway(origin, ownerKey);
    say('RELISTEN', origin);
    for (const [db, sql] of [
      ['stress_events', 'SELECT count(*) FROM events'],
      ['stress_board', 'SELECT count(*) FROM players'],
      ['board_copy', 'SELECT count(*) FROM players'],
    ]) {
      const nowCount = (await owner.database(db).execute(sql)).rows[0][0];
      assert(nowCount === counts[db], `${db} lost rows across SIGKILL: ${counts[db]} -> ${nowCount}`);
    }
    await owner.database('stress_events').execute(
      "INSERT INTO events (kind, payload, ts) VALUES ('post', 'restart', 2)"
    );
    return { ...counts, relisten: origin };
  });

  await section('T8_LOAD_BURST', async () => {
    const db = owner.database('stress_events');
    const before = (await db.execute('SELECT count(*) FROM events')).rows[0][0];
    const n = 2000;
    const durs = [];
    for (let i = 0; i < n; i++) {
      const t = Date.now();
      await db.execute('INSERT INTO events (kind, payload, ts) VALUES (?, ?, ?)', ['burst', `b${i}`, i]);
      durs.push(Date.now() - t);
    }
    durs.sort((a, b) => a - b);
    const p50 = durs[Math.floor(n * 0.5)];
    const p99 = durs[Math.floor(n * 0.99)];
    const after = (await db.execute('SELECT count(*) FROM events')).rows[0][0];
    assert(after === before + n, `burst lost writes: ${before} -> ${after} (want +${n})`);
    return { n, p50_ms: p50, p99_ms: p99, total: after };
  });

  await section('T9_BACKUP_SET_VERIFY', async () => {
    const manifest = execFileSync(
      backupScript,
      ['--server', origin, '--key-file', keyFile, '--data-dir', dataDir, '--backup-root', backupRoot, '--keep', '3'],
      { encoding: 'utf8', timeout: 120_000, env: { ...process.env, DG_BIN: binary } }
    );
    const set = JSON.parse(manifest.split('\n').find((l) => l.trim().startsWith('{')));
    assert(Array.isArray(set.databases), 'manifest missing databases');
    const names = set.databases.map((d) => d.name);
    for (const want of ['stress_events', 'stress_board', 'stress_todo']) {
      assert(names.includes(want), `${want} missing from backup set`);
    }
    const report = execFileSync(
      binary,
      ['backup', 'verify', '--backup-root', backupRoot, 'latest'],
      { encoding: 'utf8', timeout: 120_000 }
    );
    const parsed = JSON.parse(report);
    assert(parsed.ok === true, `verify not ok: ${report.slice(0, 200)}`);
    return { set: set.timestamp, databases: names, ok: parsed.ok };
  });

  say('FAILURES', failures.length === 0 ? 'none' : failures);
  say('END_EPOCH', Math.floor(Date.now() / 1000));
  say('VERDICT', failures.length === 0 ? 'pass' : 'FAIL');
  process.exitCode = failures.length === 0 ? 0 : 1;
} catch (error) {
  console.log(`[ERROR] ${error?.message ?? error}`);
  say('VERDICT', 'FAIL');
  process.exitCode = 1;
} finally {
  if (server) {
    try {
      server.kill('SIGTERM');
      await new Promise((resolve) => server.on('exit', resolve));
    } catch { /* already dead */ }
  }
  rmSync(dataDir, { recursive: true, force: true });
  rmSync(backupRoot, { recursive: true, force: true });
}
