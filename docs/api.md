# DB Gateway API and CLI reference

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

Store key files with `0600` permissions; the CLI refuses group/world-readable key files. **Automated development clients should use scoped dev credentials, never the owner key.** Each registered database also has a durable opaque generation identity. A scoped credential stores the database name together with the generation it was issued for; after tombstone purge and same-name recreation, the old credential receives `database_forbidden` while a newly issued credential can access the replacement. Legacy name-only scope metadata is migrated to the current generation without broadening access.

## Routes

| Method + path | Who | Body | Success response |
| --- | --- | --- | --- |
| `GET /health/live` | none | — | `{"status":"ok"}` |
| `POST /v1/databases` | owner | `{"name":"game_a"}` | `{"name":"game_a"}` |
| `GET /v1/databases/{name}` | owner | — | `{"name":"…","tombstoned":false,"size_bytes":n,"created_at":epoch,"tables":[{"name":"…","rows":n},…]}` (tombstoned names report `{"name":"…","tombstoned":true,"purge_after":epoch,"size_bytes":n}` instead; unknown names are `404 database_not_found`) |
| `GET /v1/databases` | owner | — | `{"databases":["game_a", …]}` |
| `POST /v1/keys` | owner | `{"name":"bot_a","role":"database_read_write","databases":["game_a"]}` (or role `database_read_only`) | `{"key":"<64 hex>"}` |
| `GET /v1/keys` | owner | — | `{"keys":[{"name":"…","role":"…","databases":[…]}]}` (hashes never shown; the owner row is listed) |
| `DELETE /v1/keys/{name}` | owner | — | `{"revoked":true}` |
| `POST /v1/databases/{name}/query` | scoped key | `{"sql":"INSERT … VALUES (?)","args":[…]}` | `{"columns":[…],"rows":[[…]],"rows_affected":n}` |
| `POST /v1/databases/{name}/batch` | scoped key | `{"atomic":true,"statements":[{"sql":…,"args":[…]},…]}` | `{"results":[ …query results… ]}` |
| `POST /v1/databases/{name}/clone` | read-write key on source | `{"destination":"name2"}` | `{"source":"…","destination":"…"}` |
| `POST /v1/databases/{name}/export` | scoped key | `{}` | `{"database":"…","sha256":"…","size":n,"data_base64":"…"}` |
| `POST /v1/databases/{name}/import` | owner | `{"name":"new_name","data_base64":"…"}` | `{"imported":"…","from":"…"}` |
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
| `sql_forbidden` | 403 | Read-only write attempt, or SQL touching protected `_dg_*` tables / disallowed features (attach, pragmas, `ALTER`, triggers/views, temp objects, virtual tables, load_extension, user transactions) |
| `database_not_found` | 404 | No such database (never silently created) |
| `invalid_request` | 400 | Malformed JSON, unknown fields, bad name, unsupported arg, empty batch, more than 32 batch statements, SQL text over 64 KiB, more than 256 args, or malformed idempotency key |
| `invalid_request` | 413 | JSON body over the 64 KiB general cap; import requests have a 12 MiB encoded transport envelope, while decoded import data is capped at 8 MiB |
| `invalid_sql` | 400 | Syntax error, wrong parameter count, unknown table/column |
| `result_limit_exceeded` | 413 | Result exceeds 256 KiB or 1000 rows |
| `idempotency_key_conflict` | 409 | Same key, different request body |
| `database_exists` | 409 | Name already registered/file exists, or tombstoned (delete not yet purged) |
| `already_bootstrapped` | 409 | Second `dg init` attempt |
| `key_exists` | 409 | Credential name reused |
| `file_exists` | 409 | Export destination or clone target already exists on disk |
| `confirmation_required` | 400 | `dg delete` invoked without a matching `--confirm NAME` |
| `capacity_exceeded` | 429 | Global task, per-database queue, credential, or replay capacity full |
| `query_deadline_exceeded` | 408 | Execution passed the 2s deadline; transaction rolled back |

## Write semantics, Idempotency-Key, retries

Every request runs in one managed transaction; a write is acknowledged only after commit (WAL, `synchronous=FULL`). A failed statement rolls back the whole request. An explicit `query_deadline_exceeded` response means the transaction rolled back. If the transport dies after a write was sent, the outcome is unknown — **never blindly retry an unkeyed write**. To make a write safely replayable, send `Idempotency-Key: <key>`: with the same key and identical body, the server replays the stored original result without re-executing. Key scope is (database, principal, key); retention is 24h, at most 256 keys per principal per database (429 `capacity_exceeded` for new keys at capacity; existing replays still work; other principals unaffected). Read-only credentials must not send the header: a keyed request on a read-only key is rejected with 400 `invalid_request`. Same key + different body = 409. The TS client's `Database.retry(sql,args,key)` is a convenience for explicit replay; resending the original keyed request through the CLI or HTTP API is also supported; no hidden retries exist in client or CLI. Details: `docs/architecture.md`.

## Lifecycle: clone, export, import, delete (M2)

Clones and exports use SQLite's **online backup API**, never raw file copying: the snapshot is a consistent committed point in time even while writes continue on the source (WAL readers do not block writers). Exported files are standalone SQLite databases in DELETE journal mode.

**Import is owner-only.** It validates the file (`PRAGMA integrity_check`, foreign-key check, no protected tables besides the server's own `_dg_replay`) and publishes it in one atomic registry step — a failed import leaves no partial database. Import targets must not collide with live **or tombstoned** names. Before publication the gateway ensures its `_dg_replay` schema exists, so **keyed writes and replays work immediately on imported databases**; ordinary external SQLite files need no preparation. A file that already carries a `_dg_replay` table is published only if its definition matches the canonical schema (a smuggled or incompatible variant is rejected), and other `_dg_*` tables are always rejected. Read-write application keys may export or clone within their scope, but cannot publish database imports.

Interruption boundary: publication links the file first and commits the registry row second. A crash between the two leaves an unregistered orphan file. After restart the orphan is invisible (`404`), `create`/import refuse to overwrite or adopt it (`409`), and an unrelated valid import still succeeds; operator recovery is deleting the orphan file and re-importing, which then publishes exact data.

**Delete is owner-only and tombstoned:** the registry row keeps `deleted_at` and a separate `purge_after` deadline. The name immediately disappears from listing and subsequent owner/scoped-key queries return `404 database_not_found`; create/import are refused (`409`) while the tombstone holds. Credentials remain valid for their other databases. Already-started requests may finish against the retained file.

The server retains the SQLite file for **24 hours** and checks expired tombstones at startup and every second, even without incoming traffic. Purge waits for request connections to close; failures are logged and retried on later ticks. File removal and registry removal are serialized with publication. The name can be created again after successful purge. Export before deleting anything important: no automated restore is provided until M3.

`dg delete NAME --confirm NAME` requires the exact name to be typed twice. Missing or mismatched confirmation fails locally with `confirmation_required` and sends no DELETE request; `--yes` is not supported. At the HTTP level, sending owner-authenticated `DELETE /v1/databases/{name}` is the explicit destructive request (no body). Lifecycle staging is private per request and automatically cleaned up on both success and error.

The HTTP export/import payload limit is 8 MiB of decoded SQLite data. The CLI accepts only regular import files within that bound and rejects larger files before base64 encoding or sending them. `dg export --out PATH` writes through a same-directory temporary file and a no-clobber publication: an existing destination is preserved and returns `file_exists`; a successful export is never exposed as a partial file.

## CLI

Non-shell command output is JSON; the shell defaults to aligned text. One exception: `dg query` renders the shell's aligned table when stdout is a terminal and `--json` is absent — piped output always stays JSON, so scripts are unaffected. Global flags: `--server <url>` (default `http://127.0.0.1:7432`; HTTPS or literal loopback only), `--key-file <path>` (required; `0600`), `--json`.

Connection resolution, highest precedence first: `--server` / `--key-file` flags, then `DG_SERVER` / `DG_KEY_FILE` environment variables, then defaults (`http://127.0.0.1:7432` and `$HOME/.config/dg/key`). With the env vars set — or the default key file in place — daily commands need no flags at all: `dg list`, `dg query game_a "SELECT …"`.

```sh
dg init --data-dir state/dg            # once per data dir; prints {"owner_key": …} — save it securely, it is shown once
dg serve --listen 127.0.0.1:7432 --data-dir state/dg
dg --key-file owner.key list
dg --key-file owner.key create game_a
dg --key-file owner.key info game_a       # size, tables+rows, created_at, tombstone state
dg --key-file owner.key key create bot_a --role database_read_write --database game_a   # prints {"key": …} once
dg --key-file owner.key key list
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
dg --key-file owner.key delete game_a_copy --confirm game_a_copy   # tombstone; file purge after 24h retention
```

Non-shell exit codes: `0` success; nonzero on any error. HTTP API errors produce JSON on stdout (the CLI also adds `status`); local validation, configuration and transport errors are logged to stderr instead. Deletion is non-interactive but requires typed-name confirmation — scripts and agents must supply the exact target name again.

### Interactive shell

```sh
dg --server http://127.0.0.1:7432 --key-file bot_a.key shell
# At the dg> prompt:
# use game_a
# SELECT id, name FROM players;
# .tables
# .schema players
# .quit
```

`use <db>` (alias `.use <db>`) selects a database for subsequent requests. Selection validates the name locally, **does not create or contact a database**, and clears any pending idempotency key. Missing, deleted, or unauthorized databases produce the ordinary API error on the next request. An invalid name leaves the previous selection unchanged. The session retains only this selection and shell settings — it is **not a persistent SQLite connection or transaction**. Each nonempty SQL line is one `/query` request with empty `args`; multiline SQL is not accumulated. Use a batch for parameterized statements or an atomic group.

| Command | Behavior |
| --- | --- |
| `.help` | List commands and retry semantics |
| `.quit` / `.exit` | Exit immediately, including during batch entry; EOF also exits |
| `.tables` | Show user tables, excluding `_dg_*` tables |
| `.schema [table]` | Show CREATE statements, optionally filtered by table name |
| `:batch [JSON]` | Submit a batch body inline or paste it on following lines; accumulates until a complete JSON value is available |
| `.` during batch entry | Cancel the batch and clear the pending key |
| `:key <key>` | Set a key for the next SQL/batch attempt; `:key` alone clears it |
| `.json` | Toggle JSON output; `--json` starts the shell in JSON mode |

Piped input works without prompts. This example emits one JSON object per result/error/notice line:

```sh
dg --key-file bot_a.key --json shell <<'SQL'
use game_a
SELECT id, name FROM players;
:batch
{
  "atomic": true,
  "statements": [
    {"sql": "SELECT name FROM players WHERE id = ?", "args": [1]},
    {"sql": "SELECT count(*) FROM players"}
  ]
}
.quit
SQL
```

Text mode aligns columns using character counts (wide/combining Unicode glyphs may not align perfectly) and escapes control characters in cells. Batch results are labeled `statement 1:`, `statement 2:`, etc. JSON mode preserves the API result shape, including `{"results":[…]}` for batches; notices use `{"message":"…"}`. API **and local session errors** use `{"error":{"code":"…","message":"…"},"status":n}` in JSON mode, or `error [n] code: message` in text mode. Prompts appear only when both stdin and stdout are terminals and text mode is active (`dg>` / `dg:game_a>`; `...>` while pasting a batch).

A failed request prints its error and the session continues; `.quit` or EOF returns `0` even if earlier statements failed. Credential/startup or input/output failures exit nonzero. For scripts needing fail-fast exit codes, use `dg query` / `dg batch` instead. Input lines and accumulated batches are capped at 64 KiB (`input_limit_exceeded`); malformed JSON produces `invalid_request`, and incomplete JSON at EOF produces an error without sending a request. Server-side SQL, body, transaction, and result limits still apply.

**No retries occur.** `:key` applies to the next SQL/batch attempt, even a read or failed attempt, then is consumed; `.tables`/`.schema` do not consume it. Cancelling a batch or switching database clears it. To explicitly replay a write after `outcome_unknown`, set the same key again and submit the identical body within the 24h retention window. Never blindly repeat an unkeyed write. The shell neither writes credentials nor saves SQL history to disk.
