// Event-log pet app: append-only time-series writes.
export async function runEventlog(db) {
  await db.execute(
    'CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL, ts INTEGER NOT NULL)'
  );
  const now = Math.floor(Date.now() / 1000);
  const kinds = ['deploy', 'ping', 'deploy', 'error', 'ping'];
  for (let i = 0; i < 50; i++) {
    const kind = kinds[i % kinds.length];
    await db.execute('INSERT INTO events (kind, payload, ts) VALUES (?, ?, ?)', [
      kind,
      `event-${i}`,
      now + i,
    ]);
  }
  const count = await db.execute('SELECT count(*) FROM events');
  const byKind = await db.execute(
    'SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind'
  );
  return {
    total: count.rows[0][0],
    byKind: byKind.rows,
  };
}

export async function verifyEventlogRestored(gateway, name) {
  const db = gateway.database(name);
  const total = await db.execute('SELECT count(*) FROM events');
  return total.rows[0][0];
}
