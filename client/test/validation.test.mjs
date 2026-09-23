import assert from 'node:assert/strict';
import http from 'node:http';
import test from 'node:test';
import { Gateway, GatewayError } from '../dist/index.js';

async function withServer(handler, run) {
  const server = http.createServer(handler);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const { port } = server.address();
    await run(new Gateway(`http://127.0.0.1:${port}`, 'a'.repeat(64)));
  } finally {
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  }
}

test('non-finite SQL numbers and malformed BLOBs fail before transport', async () => {
  let requests = 0;
  await withServer((_request, response) => {
    requests += 1;
    response.end('{}');
  }, async gateway => {
    const db = gateway.database('app');
    for (const value of [Number.NaN, Infinity, -Infinity, [256]]) {
      await assert.rejects(db.execute('SELECT ?', [value]), /Invalid SQL argument/);
      await assert.rejects(db.batch([{ sql: 'SELECT ?', args: [value] }]), /Invalid SQL argument/);
    }
  });
  assert.equal(requests, 0);
});

test('malformed single-shot export base64 is a GatewayError', async () => {
  await withServer((_request, response) => {
    response.setHeader('content-type', 'application/json');
    response.end(JSON.stringify({ database: 'app', size: 1, sha256: '0'.repeat(64), data_base64: '!' }));
  }, async gateway => {
    await assert.rejects(gateway.database('app').exportSnapshot(), error => {
      assert.ok(error instanceof GatewayError);
      assert.equal(error.code, 'outcome_unknown');
      assert.equal(error.message, 'Malformed export response');
      return true;
    });
  });
});

test('malformed chunked export base64 is a GatewayError', async () => {
  await withServer((request, response) => {
    response.setHeader('content-type', 'application/json');
    if (request.url?.startsWith('/v1/databases/app/export/begin')) {
      response.end(JSON.stringify({ snapshot: 'a'.repeat(64), size: 1, sha256: '0'.repeat(64) }));
    } else {
      response.end(JSON.stringify({ snapshot: 'a'.repeat(64), size: 1, sha256: '0'.repeat(64), offset: 0, data_base64: '!' }));
    }
  }, async gateway => {
    await assert.rejects(gateway.database('app').exportChunked(), error => {
      assert.ok(error instanceof GatewayError);
      assert.equal(error.code, 'outcome_unknown');
      assert.equal(error.message, 'Malformed export response');
      return true;
    });
  });
});
