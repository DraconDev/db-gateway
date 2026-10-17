// Leaderboard pet app: players + scores with an atomic batch seed.
export async function runLeaderboard(db) {
  await db.execute(
    'CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT NOT NULL)'
  );
  await db.execute(
    'CREATE TABLE scores (id INTEGER PRIMARY KEY, player_id INTEGER NOT NULL, points INTEGER NOT NULL, recorded_at INTEGER NOT NULL)'
  );
  const now = Math.floor(Date.now() / 1000);
  await db.batch([
    { sql: 'INSERT INTO players (name) VALUES (?)', args: ['aria'] },
    { sql: 'INSERT INTO players (name) VALUES (?)', args: ['kim'] },
    { sql: 'INSERT INTO players (name) VALUES (?)', args: ['jo'] },
    { sql: 'INSERT INTO scores (player_id, points, recorded_at) VALUES (1, 150, ?)', args: [now] },
    { sql: 'INSERT INTO scores (player_id, points, recorded_at) VALUES (2, 200, ?)', args: [now] },
    { sql: 'INSERT INTO scores (player_id, points, recorded_at) VALUES (3, 120, ?)', args: [now] },
    { sql: 'INSERT INTO scores (player_id, points, recorded_at) VALUES (1, 75, ?)', args: [now] },
  ]);
  const board = await db.execute(
    'SELECT p.name, sum(s.points) AS total FROM players p JOIN scores s ON s.player_id = p.id GROUP BY p.name ORDER BY total DESC'
  );
  return {
    rows: board.rows,
    top: board.rows[0],
  };
}

export async function verifyLeaderboardRestored(gateway, name) {
  const db = gateway.database(name);
  const total = await db.execute('SELECT count(*) FROM scores');
  return total.rows[0][0];
}
