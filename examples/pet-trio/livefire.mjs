// Live-fire driver: mirror-of-local trio workloads through scoped keys.
//
// Usage (secrets stay in 0600 files, never in the repo):
//   LIVE_ORIGIN=http://127.0.0.1:17432 LIVE_KEYS=/tmp/livefire-keys \
//     node examples/pet-trio/livefire.mjs
// Expects key files todo.key / board.key / events.key holding raw 64-hex
// credentials scoped to pet_todo_live / pet_board_live / pet_events_live.
// Exit 0 + [VERDICT] pass only if every workload verifies.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { Gateway } from '../../client/dist/index.js';
import { runTodo } from './todo.mjs';
import { runLeaderboard } from './leaderboard.mjs';
import { runEventlog } from './eventlog.mjs';

const say = (k, v) => console.log(`[${k}] ${typeof v === 'string' ? v : JSON.stringify(v)}`);
const origin = process.env.LIVE_ORIGIN;
const keysDir = process.env.LIVE_KEYS;
if (!origin || !keysDir) {
  console.log('[ERROR] set LIVE_ORIGIN and LIVE_KEYS');
  process.exitCode = 1;
} else {
  try {
    say('START_EPOCH', Math.floor(Date.now() / 1000));
    say('ORIGIN', origin);
    const key = (f) => readFileSync(join(keysDir, f), 'utf8').trim();
    const todo = new Gateway(origin, key('todo.key')).database('pet_todo_live');
    const boardDb = new Gateway(origin, key('board.key')).database('pet_board_live');
    const events = new Gateway(origin, key('events.key')).database('pet_events_live');

    say('TODO_RESULT', await runTodo(todo));
    say('BOARD_RESULT', await runLeaderboard(boardDb));
    say('EVENTS_RESULT', await runEventlog(events));

    // Re-read proof through the same scoped keys.
    say('TODO_REREAD', await todo.execute('SELECT count(*), sum(done) FROM todos'));
    say('BOARD_REREAD', await boardDb.execute('SELECT sum(points) FROM scores'));
    say('EVENTS_REREAD', await events.execute('SELECT count(*) FROM events'));
    say('END_EPOCH', Math.floor(Date.now() / 1000));
    say('VERDICT', 'pass');
  } catch (error) {
    console.log(`[ERROR] ${error?.message ?? error}`);
    say('VERDICT', 'FAIL');
    process.exitCode = 1;
  }
}
