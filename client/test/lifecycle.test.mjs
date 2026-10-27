import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Gateway, GatewayError } from '../dist/index.js';

const binary = join(import.meta.dirname, '../../.cargo/target/debug/dg');

function start(dataDir) {
  const child = spawn(binary, ['serve', '--listen', '127.0.0.1:0', '--data-dir', dataDir], { stdio: ['ignore', 'ignore', 'pipe'] });
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
  const dataDir = mkdtempSync(join(tmpdir(), 'dg-lifecycle-'));
  const { child, address } = await start(dataDir);
  try {
    const init = spawn(binary, ['init', '--data-dir', dataDir]);
    const ownerKey = JSON.parse(await new Promise((resolve, reject) => {
      let out = '';
      init.stdout.on('data', (c) => (out += c));
      init.on('exit', (code) => (code === 0 ? resolve(out) : reject(new Error(out))));
    })).owner_key;
    await run(new Gateway(`http://${address}`, ownerKey), address);
  } finally {
    child.kill('SIGTERM');
    await new Promise((resolve) => child.on('exit', resolve));
    rmSync(dataDir, { recursive: true, force: true });
  }
}

test('the schema route answers a table\'s structure', async () => {
  await withServer(async (gateway, address) => {
    await gateway.createDatabase('shop');
    await gateway.database('shop').execute(
      'CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT NOT NULL, price_cents INTEGER DEFAULT 0)',
    );
    await gateway.database('shop').execute(
      'CREATE TABLE orders (id INTEGER PRIMARY KEY, product_id INTEGER REFERENCES products(id) ON DELETE CASCADE)',
    );

    const schema = await gateway.schema('shop');
    assert.equal(schema.database, 'shop');
    assert.deepEqual(schema.tables.map((t) => t.name), ['orders', 'products']);
    const products = schema.tables.find((t) => t.name === 'products');
    assert.deepEqual(products.columns.map((c) => c.name), ['id', 'sku', 'price_cents']);
    assert.equal(products.columns[0].type, 'INTEGER');
    assert.equal(products.columns[0].primary_key, true);
    assert.equal(products.columns[1].not_null, true);
    assert.equal(products.columns[2].default, '0');
    const orders = schema.tables.find((t) => t.name === 'orders');
    assert.equal(orders.foreign_keys[0].from, 'product_id');
    assert.equal(orders.foreign_keys[0].table, 'products');
    assert.equal(orders.foreign_keys[0].on_delete, 'CASCADE');

    // A table narrows the answer, on the server.
    const one = await gateway.schema('shop', 'products');
    assert.deepEqual(one.tables.map((t) => t.name), ['products']);

    // A scoped app key may not read a database's structure: the route
    // is owner-only, like the listing beside it.
    const ro = await gateway.issueKey({ name: 'reader', role: 'database_read_only', databases: ['shop'] });
    const roGw = new Gateway(`http://${address}`, ro.key);
    await assert.rejects(roGw.schema('shop'), (e) => e instanceof GatewayError && e.code === 'forbidden');
  });
});

test('typed management: create/list/info/keys/delete plus denials', async () => {
  await withServer(async (gateway, address) => {
    assert.deepEqual(await gateway.createDatabase('shop'), { name: 'shop' });
    assert.deepEqual(await gateway.listDatabases(), ['shop']);
    const info = await gateway.databaseInfo('shop');
    assert.equal(info.tombstoned, false);
    assert.ok(info.size_bytes > 0 && Array.isArray(info.tables));

    const rw = await gateway.issueKey({ name: 'app', role: 'database_read_write', databases: ['shop'] });
    assert.match(rw.key, /^[a-f0-9]{64}$/);
    const ro = await gateway.issueKey({ name: 'reader', role: 'database_read_only', databases: ['shop'] });
    const keys = await gateway.listKeys();
    assert.deepEqual(
      keys.map((k) => [k.name, k.role, k.databases]),
      [['app', 'database_read_write', ['shop']], ['owner', 'owner', []], ['reader', 'database_read_only', ['shop']]],
    );

    const roGw = new Gateway(`http://${address}`, ro.key);
    await assert.rejects(roGw.deleteDatabase('shop'), (e) => e instanceof GatewayError && e.code === 'forbidden');
    await assert.rejects(roGw.databaseInfo('shop'), (e) => e instanceof GatewayError && e.code === 'forbidden');
    assert.throws(() => gateway.database('Bad-Name'), /Invalid database name/);
    assert.throws(() => gateway.database(''), /Invalid database name/);

    assert.deepEqual(await gateway.revokeKey('reader'), { revoked: true });
    await assert.rejects(roGw.database('shop').execute('SELECT 1'), (e) => e.code === 'invalid_credential');

    const deleted = await gateway.deleteDatabase('shop');
    assert.equal(deleted.deleted, true);
    assert.equal(deleted.purge_after_seconds, 86400);
    const tomb = await gateway.databaseInfo('shop');
    assert.equal(tomb.tombstoned, true);
    assert.deepEqual(await gateway.listDatabases(), []);
  });
});

test('typed lifecycle: clone, single-shot vs chunked export, chunked import', async () => {
  await withServer(async (gateway, address) => {
    await gateway.createDatabase('media');
    const db = gateway.database('media');
    await db.execute('CREATE TABLE blobs (id INTEGER PRIMARY KEY, v BLOB NOT NULL)');
    await db.execute('INSERT INTO blobs (v) VALUES (zeroblob(900000)), (zeroblob(900000)), (zeroblob(900000))');

    const clone = await db.cloneTo('media_copy');
    assert.deepEqual(clone, { source: 'media', destination: 'media_copy' });
    assert.equal((await gateway.database('media_copy').execute('SELECT length(v) FROM blobs')).rows[0][0], 900000);

    // Both export paths must produce byte-identical snapshots.
    const single = await db.exportSnapshot();
    assert.equal(single.database, 'media');
    assert.ok(single.size > 2000000 && single.size < 8 * 1024 * 1024);
    const chunked = await db.exportChunked();
    assert.equal(chunked.size, single.size);
    assert.equal(chunked.sha256, single.sha256);
    assert.deepEqual(chunked.data, single.data);
    // The client must release retained snapshots; eight successful exports
    // must not exhaust the server's transfer slots.
    for (let i = 0; i < 8; i++) await db.exportChunked();

    await gateway.importChunked('media_chunked', chunked.data);
    assert.equal((await gateway.database('media_chunked').execute('SELECT length(v) FROM blobs')).rows[0][0], 900000);
    await gateway.importSnapshot('media_single', single.data);
    assert.equal((await gateway.database('media_single').execute('SELECT count(*) FROM blobs')).rows[0][0], 3);
  });
});
