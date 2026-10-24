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

test('malformed chunked export metadata releases the retained snapshot', async () => {
  const snapshot = 'a'.repeat(64);
  const requests = [];
  await withServer((request, response) => {
    requests.push(`${request.method} ${request.url}`);
    response.setHeader('content-type', 'application/json');
    response.end(JSON.stringify({ snapshot, size: 1, sha256: 'not-a-hash' }));
  }, async gateway => {
    await assert.rejects(gateway.database('app').exportChunked(), error => {
      assert.ok(error instanceof GatewayError);
      assert.equal(error.code, 'outcome_unknown');
      assert.equal(error.message, 'Malformed export begin response');
      return true;
    });
  });
  assert.deepEqual(requests, [
    'POST /v1/databases/app/export/begin',
    `DELETE /v1/snapshots/${snapshot}`,
  ]);
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

test('a status answer with a placement is read, and a malformed one is refused', async () => {
  const base = {
    version: '0.2.0',
    uptime_secs: 10,
    admission: { acquire_timeout_ms: 500, read_permits: 8, write_permits: 4 },
    requests: { total: 0, capacity_exceeded: 0, query_deadline_exceeded: 0, internal_error: 0 },
  };
  const withBody = body => async (_request, response) => {
    response.setHeader('content-type', 'application/json');
    response.end(JSON.stringify(body));
  };

  // The placement is optional: a gateway that does not report it still
  // validates, because the question is then unanswerable, not answered
  // with blanks.
  await withServer(withBody(base), async gateway => {
    const status = await gateway.status();
    assert.equal(status.deployment, undefined);
  });

  await withServer(withBody({
    ...base,
    deployment: {
      host: 'micro2',
      listen: '127.0.0.1:7432',
      data_dir: '/var/lib/dg',
      backup_root: '/var/lib/dg-backups',
      databases: {
        alpha: { file: '/var/lib/dg/alpha.sqlite', exists: true, size_bytes: 4096, tombstoned: false },
      },
    },
  }), async gateway => {
    const status = await gateway.status();
    assert.equal(status.deployment.host, 'micro2');
    assert.equal(status.deployment.data_dir, '/var/lib/dg');
    assert.equal(status.deployment.databases.alpha.exists, true);
  });

  // A placement that does not make sense is refused, not passed on.
  await withServer(withBody({ ...base, deployment: { listen: 7432 } }), async gateway => {
    await assert.rejects(gateway.status(), /Malformed status response/);
  });
});
