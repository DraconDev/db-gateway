// Pet-trio runner: todo + leaderboard + event log against a disposable
// local gateway, each behind its own scoped key, plus export/re-import
// proof per database (the local shape of "CRUD + backups").
//
// Usage: node examples/pet-trio/run-pet-trio.mjs
// Exit 0 only if every app verifies and every restored copy matches.
// Transcript goes to stdout; save as docs/pet-trio-<date>.log.
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Gateway } from '../../client/dist/index.js';
import { runTodo, verifyTodoRestored } from './todo.mjs';
import { runLeaderboard, verifyLeaderboardRestored } from './leaderboard.mjs';
import { runEventlog, verifyEventlogRestored } from './eventlog.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..');
const binary = join(root, '.cargo', 'target', 'debug', 'dg');

const say = (k, v) => console.log(`[${k}] ${typeof v === 'string' ? v : JSON.stringify(v)}`);

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

const dataDir = mkdtempSync(join(tmpdir(), 'dg-pet-trio-'));
let server = null;
try {
  say('START_EPOCH', Math.floor(Date.now() / 1000));
  const started = await start(dataDir);
  server = started.child;
  say('LISTEN', started.address);
  const origin = `http://${started.address}`;

  // Bootstrap the owner key into a 0600 file (same shape as docs).
  const { execFileSync } = await import('node:child_process');
  const out = execFileSync(binary, ['init', '--data-dir', dataDir], { encoding: 'utf8' });
  const ownerKey = JSON.parse(out).owner_key;
  const keyFile = join(dataDir, 'owner.key');
  writeFileSync(keyFile, ownerKey + '\n');
  chmodSync(keyFile, 0o600);
  const owner = new Gateway(origin, ownerKey);

  const apps = [
    { db: 'pet_todo', key: 'pet_todo_app', run: runTodo, verify: verifyTodoRestored, label: 'TODO' },
    { db: 'pet_leaderboard', key: 'pet_board_app', run: runLeaderboard, verify: verifyLeaderboardRestored, label: 'BOARD' },
    { db: 'pet_events', key: 'pet_events_app', run: runEventlog, verify: verifyEventlogRestored, label: 'EVENTS' },
  ];

  for (const app of apps) {
    await owner.request('/v1/databases', 'POST', { name: app.db });
    const issued = await owner.request('/v1/keys', 'POST', {
      name: app.key,
      role: 'database_read_write',
      databases: [app.db],
    });
    const scoped = new Gateway(origin, issued.key);
    const result = await app.run(scoped.database(app.db));
    say(`${app.label}_RESULT`, result);
    // Scoped key sees only its own database.
    const visible = await scoped.listDatabases();
    if (visible.length !== 1 || visible[0] !== app.db) {
      throw new Error(`${app.label}: scoped key sees ${JSON.stringify(visible)}`);
    }
    say(`${app.label}_SCOPED_OK`, visible);
  }

  // Export + re-import proof per database (backup-capable path).
  for (const app of apps) {
    const snap = await owner.database(app.db).exportSnapshot();
    say(`${app.label}_EXPORT`, { size: snap.size, sha256: snap.sha256.slice(0, 16) + '…' });
    const restored = `${app.db}_restored`;
    await owner.importSnapshot(restored, snap.data);
    const liveCount =
      app.label === 'TODO'
        ? (await owner.database(app.db).execute('SELECT count(*) FROM todos')).rows[0][0]
        : app.label === 'BOARD'
          ? (await owner.database(app.db).execute('SELECT count(*) FROM scores')).rows[0][0]
          : (await owner.database(app.db).execute('SELECT count(*) FROM events')).rows[0][0];
    const backCount = await app.verify(owner, restored);
    if (liveCount !== backCount) throw new Error(`${app.label}: restored ${backCount} != live ${liveCount}`);
    say(`${app.label}_RESTORE_OK`, { live: liveCount, restored: backCount });
  }

  say('END_EPOCH', Math.floor(Date.now() / 1000));
  say('VERDICT', 'pass');
} catch (error) {
  console.log(`[ERROR] ${error?.message ?? error}`);
  say('VERDICT', 'FAIL');
  process.exitCode = 1;
} finally {
  if (server) {
    server.kill('SIGTERM');
    await new Promise((resolve) => server.on('exit', resolve));
  }
  rmSync(dataDir, { recursive: true, force: true });
}
