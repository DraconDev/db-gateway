// Todo pet app: personal task list over a scoped database handle.
// All SQL goes through the app-scoped key — never the owner key.
export async function runTodo(db) {
  await db.execute(
    'CREATE TABLE todos (id INTEGER PRIMARY KEY, title TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)'
  );
  const batch = await db.batch([
    { sql: 'INSERT INTO todos (title) VALUES (?)', args: ['water the plants'] },
    { sql: 'INSERT INTO todos (title) VALUES (?)', args: ['read dg api docs'] },
    { sql: 'INSERT INTO todos (title) VALUES (?)', args: ['ship pet trio'] },
  ]);
  if (batch.results.length !== 3) throw new Error('todo batch short');
  await db.execute('UPDATE todos SET done = 1 WHERE title = ?', ['water the plants']);
  const total = await db.execute('SELECT count(*) FROM todos');
  const done = await db.execute('SELECT count(*) FROM todos WHERE done = 1');
  const titles = await db.execute('SELECT title FROM todos ORDER BY id');
  return {
    total: total.rows[0][0],
    done: done.rows[0][0],
    titles: titles.rows.map((r) => r[0]),
  };
}

export async function verifyTodoRestored(gateway, name) {
  const db = gateway.database(name);
  const total = await db.execute('SELECT count(*) FROM todos');
  return total.rows[0][0];
}
