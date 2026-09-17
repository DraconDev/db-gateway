# DB Gateway API and CLI reference (M1)

All routes are JSON over HTTP. Listening is loopback-only in M1; production requires HTTPS or a private network (deployment is a separate, approved milestone). All errors use one shape:

```json
{ "error": { "code": "stable_error_code", "message": "human-readable detail" } }
```

Authentication is `Authorization: Bearer <credential>` on every route. Credentials are 64-char hex strings, generated server- or CLI-side, stored only as SHA-256 hashes. There is no unauthenticated route except `GET /health/live` (returns `{"status":"ok"}`; liveness only — not database or backup readiness).

## Credential model

| Role | Obtained via | Allows |
| --- | --- | --- |
| `owner` | `dg init --data-dir <dir>` (local, printed once) | All management routes + SQL on all databases |
| `database_read_only` | `dg key create` (owner) | SQL reads on its scoped databases |
| `database_read_write` | `dg key create` (owner) | SQL reads + writes on its scoped databases |

Store key files with `0600` permissions; the CLI refuses group/world-readable key files. **Automated development clients should use scoped dev credentials, never the owner key.**

## Routes

| Method + path | Who | Body | Success response |
| --- | --- | --- | --- |
| `GET /health/live` | none | — | `{"status":"ok"}` |
| `POST /v1/databases` | owner | `{"name":"game_a"}` | `{"name":"game_a"}` |
| `GET /v1/databases` | owner | — | `{"databases":["game_a", …]}` |
| `POST /v1/keys` | owner | `{"name":"bot-a","role":"database_read_only\|database_read_write","databases":["game_a"]}` | `{"key":"<64 hex>","name":"bot-a","role":…,"databases":[…]}` |
| `DELETE /v1/keys/{name}` | owner | — | `{"revoked":true}` |
| `POST /v1/databases/{name}/query` | scoped key | `{"sql":"INSERT … VALUES (?)","args":[…]}` | `{"columns":[…],"rows":[[…]],"rows_affected":n}` |
| `POST /v1/databases/{name}/batch` | scoped key | `{"atomic":true,"statements":[{"sql":…,"args":[…]},…]}` | `{"results":[ …query results… ]}` |

Names: `[a-z0-9_]+`, not starting with `_`, max 64 chars. Queries never create databases — a missing database is `404 database_not_found`.

### SQL value encoding

Arguments and returned values map JSON ↔ SQLite: `null→NULL`, `boolean→0/1`, `number→INTEGER/REAL` (unsigned integers above `2^63-1` are rejected), `string→TEXT`, `array of 0–255 integers→BLOB` (returned as arrays of numbers). Objects are rejected.

### Errors (stable codes)

| Code | HTTP | Meaning |
| --- | --- | --- |
| `invalid_credential` | 401 | Missing/unknown/revoked key |
| `forbidden` | 403 | App key on a management route |
| `database_forbidden` | 403 | Key not scoped to this database |
| `sql_forbidden` | 403 | Read-only write attempt, or SQL touching protected `_dg_*` tables / disallowed features (attach, pragmas, triggers/views, virtual tables, load_extension, user transactions) |
| `database_not_found` | 404 | No such database (never silently created) |
| `invalid_request` | 400 | Malformed JSON, unknown fields, bad name, unsupported arg |
| `invalid_sql` | 400 | Syntax error, wrong parameter count, unknown table/column |
| `result_limit_exceeded` | 413 | Result exceeds 256 KiB or 1000 rows |
| `idempotency_key_conflict` | 409 | Same key, different request body |
| `database_exists` | 409 | Name already registered/file exists |
| `already_bootstrapped` | 409 | Second `dg init` attempt |
| `key_exists` | 409 | Credential name reused |
| `capacity_exceeded` | 429 | Global task, per-database queue, credential, or replay capacity full |
| `query_deadline_exceeded` | 408 | Execution passed the 2s deadline; transaction rolled back |

## Write semantics, Idempotency-Key, retries

Every request runs in one managed transaction; a write is acknowledged only after commit (WAL, `synchronous=FULL`). A failed statement rolls back the whole request. If the transport dies or the 2s deadline fires after a write was sent, the outcome is unknown — **never blindly retry an unkeyed write**. To make a write safely replayable, send `Idempotency-Key: <key>`: with the same key and identical body, the server replays the stored original result without re-executing. Key scope is (database, principal, key); retention is 24h, at most 256 keys per principal per database (429 `capacity_exceeded` for new keys at capacity; existing replays still work; other principals unaffected). Same key + different body = 409. The TS client's `Database.retry(sql,args,key)` is the only sanctioned replay path; no hidden retries exist in client or CLI. Details: `docs/architecture.md`.

## CLI

All output is JSON. Global flags: `--server <url>` (default `http://127.0.0.1:7432`; HTTPS or literal loopback only), `--key-file <path>` (required; `0600`), `--json`.

```sh
dg init --data-dir state/dg            # once per data dir; prints {"owner_key": …} — save it securely, it is shown once
dg serve --listen 127.0.0.1:7432 --data-dir state/dg
dg --key-file owner.key list
dg --key-file owner.key create game_a
dg --key-file owner.key key create bot-a --role database_read_write --database game_a   # prints {"key": …} once
dg --key-file owner.key query game_a "INSERT INTO players (name) VALUES (?)" --args '["aria"]'
dg --key-file owner.key query game_a "SELECT id, name FROM players"
dg --key-file owner.key query game_a "INSERT INTO matches VALUES (?)" --args '[7]' --idempotency-key match-final-9182
dg --key-file owner.key batch game_a batch.json          # {"atomic":true,"statements":[…]}
dg --key-file owner.key key revoke bot-a
```

Exit codes: `0` success; nonzero on any error, with the stable error JSON on stdout. Destructive actions are not interactive in M1 — scripts and agents should call deliberately.
