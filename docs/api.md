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
| `POST /v1/keys` | owner | `{"name":"bot_a","role":"database_read_write","databases":["game_a"]}` (or role `database_read_only`) | `{"key":"<64 hex>"}` |
| `DELETE /v1/keys/{name}` | owner | — | `{"revoked":true}` |
| `POST /v1/databases/{name}/query` | scoped key | `{"sql":"INSERT … VALUES (?)","args":[…]}` | `{"columns":[…],"rows":[[…]],"rows_affected":n}` |
| `POST /v1/databases/{name}/batch` | scoped key | `{"atomic":true,"statements":[{"sql":…,"args":[…]},…]}` | `{"results":[ …query results… ]}` |
| `POST /v1/databases/{name}/clone` | read-write key on source | `{"destination":"name2"}` | `{"source":"…","destination":"…"}` |
| `POST /v1/databases/{name}/export` | scoped key | `{}` | `{"database":"…","sha256":"…","size":n,"data_base64":"…"}` |
| `POST /v1/databases/{name}/import` | read-write key on path database | `{"name":"new_name","data_base64":"…"}` | `{"imported":"…","from":"…"}` |
| `DELETE /v1/databases/{name}` | owner | — | `{"deleted":true,"purge_after_seconds":86400}` |

Names: `[a-z0-9_]+`, not starting with `_`, max 64 chars. Queries never create databases — a missing database is `404 database_not_found`.

### SQL value encoding

Arguments and returned values map JSON ↔ SQLite: `null→NULL`, `boolean→0/1`, `number→INTEGER/REAL` (unsigned integers above `2^63-1` are rejected), `string→TEXT`, `array of 0–255 integers→BLOB` (returned as arrays of numbers). Objects (including `{ "integer": "42" }`) are rejected. The TypeScript client uses JavaScript numbers: integers outside ±(2^53−1) are not guaranteed exact. For exact large identifiers, store and transmit decimal TEXT; there is no integer-wrapper encoding in M1.

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
| `database_exists` | 409 | Name already registered/file exists, or tombstoned (delete not yet purged) |
| `already_bootstrapped` | 409 | Second `dg init` attempt |
| `key_exists` | 409 | Credential name reused |
| `file_exists` | 409 | Export destination or clone target already exists on disk |
| `confirmation_required` | 400 | `dg delete` invoked without `--yes` |
| `capacity_exceeded` | 429 | Global task, per-database queue, credential, or replay capacity full |
| `query_deadline_exceeded` | 408 | Execution passed the 2s deadline; transaction rolled back |

## Write semantics, Idempotency-Key, retries

Every request runs in one managed transaction; a write is acknowledged only after commit (WAL, `synchronous=FULL`). A failed statement rolls back the whole request. An explicit `query_deadline_exceeded` response means the transaction rolled back. If the transport dies after a write was sent, the outcome is unknown — **never blindly retry an unkeyed write**. To make a write safely replayable, send `Idempotency-Key: <key>`: with the same key and identical body, the server replays the stored original result without re-executing. Key scope is (database, principal, key); retention is 24h, at most 256 keys per principal per database (429 `capacity_exceeded` for new keys at capacity; existing replays still work; other principals unaffected). Same key + different body = 409. The TS client's `Database.retry(sql,args,key)` is a convenience for explicit replay; resending the original keyed request through the CLI or HTTP API is also supported; no hidden retries exist in client or CLI. Details: `docs/architecture.md`.

## Lifecycle: clone, export, import, delete (M2)

Clones and exports use SQLite's **online backup API**, never raw file copying: the snapshot is a consistent committed point in time even while writes continue on the source (WAL readers do not block writers). Exported files are standalone SQLite databases in DELETE journal mode.

**Import** validates the file (`PRAGMA integrity_check`, foreign-key check, no protected tables besides the server's own `_dg_replay`) and publishes it in one atomic registry step — a failed import leaves no partial database. Import targets must not collide with live **or tombstoned** names.

**Delete is owner-only and tombstoned:** the registry row keeps `deleted_at`/`purge_after` so the name is immediately unqueryable (`404 database_not_found`), uncreatable (`409`), and unimportable; the SQLite file remains on disk untouched for **24 hours** (retention) and is purged opportunistically by the server after that. Until M3 backups exist, this window is the only recovery path — export before deleting anything important. `dg delete` requires `--yes`.

## CLI

All output is JSON. Global flags: `--server <url>` (default `http://127.0.0.1:7432`; HTTPS or literal loopback only), `--key-file <path>` (required; `0600`), `--json`.

```sh
dg init --data-dir state/dg            # once per data dir; prints {"owner_key": …} — save it securely, it is shown once
dg serve --listen 127.0.0.1:7432 --data-dir state/dg
dg --key-file owner.key list
dg --key-file owner.key create game_a
dg --key-file owner.key key create bot_a --role database_read_write --database game_a   # prints {"key": …} once
dg --key-file owner.key query game_a "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT)"
dg --key-file owner.key query game_a "CREATE TABLE matches (id INTEGER PRIMARY KEY)"
dg --key-file owner.key query game_a "INSERT INTO players (name) VALUES (?)" --args '["aria"]'
dg --key-file owner.key query game_a "SELECT id, name FROM players"
dg --key-file owner.key query game_a "INSERT INTO matches VALUES (?)" --args '[7]' --idempotency-key match-final-9182
printf '%s\n' '{"atomic":true,"statements":[{"sql":"SELECT count(*) FROM players"}]}' > batch.json
dg --key-file owner.key batch game_a batch.json
dg --key-file owner.key key revoke bot_a
```

Lifecycle commands (M2):

```sh
dg --key-file owner.key clone game_a game_a_copy
dg --key-file owner.key export game_a --out ./game_a_snapshot.sqlite   # writes the file, prints {name,out,size,sha256}
dg --key-file owner.key import restored --from ./game_a_snapshot.sqlite
dg --key-file owner.key delete game_a_copy --yes   # tombstone; file purge after 24h retention
```

Exit codes: `0` success; nonzero on any error. HTTP API errors produce JSON on stdout (the CLI also adds `status`); local validation, configuration and transport errors are logged to stderr instead. Destructive actions are not interactive in M1 — scripts and agents should call deliberately.
