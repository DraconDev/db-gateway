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

Store key files with `0600` permissions; the CLI refuses group/world-readable key files. `dg init` prints JSON (`{"owner_key": …}`), but `--key-file` wants the raw 64-hex credential alone — extract it once, e.g. `dg init --data-dir state/dg | python3 -c "import json,sys; print(json.load(sys.stdin)['owner_key'])" > owner.key && chmod 600 owner.key`. **Automated development clients should use scoped dev credentials, never the owner key.** Rotate a compromised or stale owner key with `dg key rotate-owner` (prints `{"key": …}` once — redirect to a `0600` file): the old key dies immediately while databases, generations, and app-key scopes are untouched. Verify the new key (`dg --key-file new.key key list`) before discarding the old file. Losing the current owner key locks out management until it is recovered from custody — app keys keep working, but nothing can issue, revoke, create, or delete. Each registered database also has a durable opaque generation identity. A scoped credential stores the database name together with the generation it was issued for; after tombstone purge and same-name recreation, the old credential receives `database_forbidden` while a newly issued credential can access the replacement. Legacy name-only scope metadata is migrated to the current generation without broadening access.

## Application clients

Applications do not hand-roll HTTP against the routes below. Two typed
clients encode the transport rules (HTTPS-or-loopback origins, no
proxies/redirects, bounded responses, `outcome_unknown` write
semantics, explicit keyed replay — never hidden retries). A read that
is given up on costs a retry and nothing else, so it gives up in ten
seconds; a write waits a minute, because giving up on one leaves the
outcome in doubt and the answer is what removes the doubt.

What makes a request a read is the request's own shape, not its HTTP
method — a query is a POST either way. The CLI, the terminal view, the
Rust client and the TypeScript client all classify the statements with
the same rule the server uses, so a read-only query is answered as a
read: it gives up in ten seconds and is reported as an unavailable
gateway, never as a write that may have committed. A management route
carries no statements to classify, so the method is the answer there
(a `GET` reads; every management route that changes something is a
`POST`/`DELETE`). Each client exports the rule it applies
(`dg_client::is_read_only`, `isReadOnly`/`semanticsOf` from
`client/`), so the classification can be checked against the server's
own rather than taken on trust.

- **TypeScript** — `client/` (`Gateway` / `Database`).
- **Rust** — `crates/dg-client` (`dg_client::Gateway` /
  `dg_client::Database`), the path for the platform's Rust writers
  (audit A6). It mirrors the TS contract: `execute` /
  `execute_with_key`, atomic `batch` / `batch_with_key`, `list_databases`,
  `status`, `database_info`, owner-only `create_database` /
  `delete_database`, and `Gateway::from_key_file` for 0600 scoped-key
  files. Key issue/revoke stays with the owner CLI. It classifies a
  query and a batch by the statements it sends
  (`dg_client::semantics_of` / `dg_client::semantics_of_statements`),
  with the same rule the server uses, so a read-only query gives up in
  ten seconds with `transport_unavailable` and never with
  `outcome_unknown`. Integration proof: `tests/rust_client.rs` drives a
  live `dg serve` end to end (scoped reads, keyed-replay
  non-duplication, out-of-scope 403) and pins the read/write
  classification against the server's own rule.

A Rust writer shaped like `api_core::db::SharedDb` keeps its
connection-supervisor role but swaps the libSQL remote connection for
a `dg_client::Gateway` built once at startup plus one scoped key file
per database; there are no streams to rebuild — a failed read is
`transport_unavailable`, a failed unkeyed write is `outcome_unknown`,
and exactly-once resubmission is the caller's explicit keyed replay.

### The agent surface (`dg mcp`)

An AI agent needs the same gateway the operator uses, and it needs that
gateway to behave the same way for both of them. `dg mcp` serves the
gateway's read surfaces to an MCP client over stdio, speaking JSON-RPC
2.0 by hand — the wire format is a handful of JSON objects, and a
dependency would cost more than the code, which matters in a project
that keeps a deliberately tiny supply chain (`cargo deny` gates it).

Two rules shape the whole surface:

- **Read-only.** The catalogue is the gateway's read surfaces and
  nothing else: `dg_status`, `dg_list`, `dg_info`, `dg_inspect`,
  `dg_backups` and `dg_query`. The write paths — create, delete, clone,
  import, execute, batch and the key mutations — are not exposed at all,
  and `dg_query` rejects a statement that would write before it sends
  anything, so an agent talking to this server cannot ask it to change
  anything.
- **Stdout is the transport.** Every byte on stdout is a JSON-RPC
  message, because stdout is how the client and the server talk: a
  stray print breaks the session. Diagnostics go to stderr, through
  tracing, like the rest of the project's logs.

The credential is the one the CLI already resolved — `--key-file`, then
`--context`, then `DG_KEY_FILE`, then `~/.config/dg/key` — read with the
same reader the CLI uses, so the file's permissions are checked the same
way. The agent never supplies a key, so a prompt that asks for one
cannot make the server hand over a secret. Results are bounded and a
truncated one says it was truncated; a 64-hex string in a result is
redacted, because a query's answer can carry a key-shaped value that
the agent has no business reading.

```sh
dg mcp                 # speaks JSON-RPC on stdin/stdout
```

## Routes

| Method + path | Who | Body | Success response |
| --- | --- | --- | --- |
| `GET /health/live` | none | — | `{"status":"ok"}` |
| `POST /v1/databases` | owner | `{"name":"game_a"}` | `{"name":"game_a"}` |
| `GET /v1/databases/{name}` | owner | — | `{"name":"…","tombstoned":false,"size_bytes":n,"created_at":epoch,"tables":[{"name":"…","rows":n},…]}` (tombstoned names report `{"name":"…","tombstoned":true,"purge_after":epoch,"size_bytes":n}` instead; unknown names are `404 database_not_found`) |
| `GET /v1/databases` | any key | — | `{"databases":["game_a", …]}` (owner: all live databases; app key: only its authorized live databases) |
| `POST /v1/keys` | owner | `{"name":"bot_a","role":"database_read_write","databases":["game_a"]}` (or role `database_read_only`) | `{"key":"<64 hex>"}` |
| `GET /v1/keys` | owner | — | `{"keys":[{"name":"…","role":"…","databases":[…]}]}` (hashes never shown; the owner row is listed) |
| `DELETE /v1/keys/{name}` | owner | — | `{"revoked":true}` |
| `POST /v1/keys/owner/rotate` | owner | — | `{"key":"<64 hex>"}` |
| `POST /v1/databases/{name}/query` | scoped key | `{"sql":"INSERT … VALUES (?)","args":[…]}` | `{"columns":[…],"rows":[[…]],"rows_affected":n}` |
| `POST /v1/databases/{name}/batch` | scoped key | `{"atomic":true,"statements":[{"sql":…,"args":[…]},…]}` | `{"results":[ …query results… ]}` |
| `POST /v1/databases/{name}/clone` | owner | `{"destination":"name2"}` | `{"source":"…","destination":"…"}` |
| `POST /v1/databases/{name}/export` | scoped key | `{}` | `{"database":"…","sha256":"…","size":n,"data_base64":"…"}` |
| `POST /v1/databases/{name}/import` | owner | `{"name":"new_name","data_base64":"…"}` | `{"imported":"…","from":"…"}` |
| `POST /v1/databases/{name}/export/begin` | scoped key | `{}` | `{"snapshot":"…","size":n,"sha256":"…"}` |
| `GET /v1/snapshots/{id}?offset=&length=` | creator/owner | — | `{"snapshot":"…","size":n,"sha256":"…","offset":n,"data_base64":"…"}` |
| `DELETE /v1/snapshots/{id}` | creator/owner | — | `{"released":true}` |
| `POST /v1/databases/{name}/import/begin` | owner | `{}` | `{"upload":"…"}` |
| `GET /v1/uploads/{id}` | owner | — | `{"upload":"…","received":n,"size_cap":n}` |
| `POST /v1/uploads/{id}/chunk` | owner | `{"offset":n,"data_base64":"…"}` | `{"upload":"…","received":n}` |
| `POST /v1/uploads/{id}/commit` | owner | `{"name":"…"}` | `{"imported":"…"}` |
| `DELETE /v1/uploads/{id}` | owner | — | `{"aborted":true}` |
| `DELETE /v1/databases/{name}` | owner | — | `{"deleted":true,"purge_after_seconds":86400}` |
| `GET /v1/status` | owner | — | `{"version":"…","uptime_secs":n,"admission":{"acquire_timeout_ms":500,"read_permits":8,"write_permits":4},"requests":{"total":n,"capacity_exceeded":n,"query_deadline_exceeded":n,"internal_error":n},"databases":{"db":{"rows_read":n,"rows_written":n,"queue_waits":n,"queue_wait_ms":n,"queue_timeouts":n}},"deployment":{"host":"…"/null,"listen":"…","data_dir":"…","backup_root":"…"/null,"databases":{"db":{"file":"…","exists":bool,"size_bytes":n,"tombstoned":bool}}}}` |
| `GET /v1/backups` | owner | — | `{"backup_root":"…"/null,"stale_after_secs":300,"cadence_secs":n/null,"sets":[{"timestamp":"…","databases":[…],"has_registry":bool,"coverage_start":n/null,"coverage_end":n/null}],"per_database":{"game_a":{"state":"fresh"/"behind"/"stale"/"unknown","newest_set":"…"/null,"age_secs":n/null,"lag_secs":n/null}},"deleted":n,"last_attempt":{…}/null,"last_failure":{…}/null,"coverage":{…}/null,"replica":{…}/null,"drill":{…}/null,"restore_points":{…}/null,"last_restore":{…}/null}` |
| `GET /v1/queries` | owner | — | `{"databases":[{"database":"…","shapes":[{"shape":"SELECT … WHERE id = ?","count":n,"rows_read":n,"rows_written":n,"total_micros":n,"max_micros":n,"first_seen":epoch,"last_seen":epoch,"principals":{"name":n}}]}],"deleted":n}` (`?database=<name>` narrows it to one database; `deleted` counts the databases whose shapes the in-memory log still holds after they were deleted) |
| `POST /v1/backups/run` | owner (read-write) | `{}` | `{"set":"20261024T195500Z","databases":[{"name":"…","size":n,"sha256":"…"}],"registry":{…},"started_at":epoch,"completed_at":epoch,"duration_ms":n}` |
| `GET /v1/audit` | owner | — | `{"entries":[{"at":epoch,"principal":"…","role":"…","action":"query","database":"…","detail":"…"}]}` (`?principal=<name>` and `?since=<epoch>` narrow it) |
| `POST /v1/diff` | owner (read access to both sides) | `{"left":"game_a","right":"game_b"}` | `{"left":"…","right":"…","identical":bool,"schema":{…},"tables":[…],"compared_rows":n,"truncated":bool}` |
| `POST /v1/branch` | owner | `{"parent":"game_a","name":"game_a_trial","at":"2026-10-16T00:00:00Z"}` (`at` optional) | `{"name":"…","parent":"…","forked_at":epoch,"source":"live"/"snapshot","set":"…"}` |
| `GET /v1/branches` | owner | — | `{"branches":[{"name":"…","parent":"…","forked_at":epoch,"source":"live"/"snapshot","set":"…"}]}` |
| `GET /v1/branches/{name}` | owner | — | one branch record |
| `DELETE /v1/branches/{name}` | owner | — | `{"deleted":true,"forgotten":true}` |
| `GET /v1/databases/{name}/integrity` | owner | — | `{"database":"…","ok":bool,"integrity":["ok"],"foreign_keys":[]}` (the messages are SQLite's own, verbatim) |
| `POST /v1/databases/{name}/vacuum` | owner (read-write) | `{}` | `{"database":"…","size_before":n,"size_after":n,"reclaimed":n,"elapsed_ms":n}` |
| `POST /v1/databases/{name}/explain` | scoped key (read) | `{"sql":"SELECT …"}` | `{"database":"…","plan":[{"id":n,"parent":n,"detail":"SCAN t"}]}` |

Names: `[a-z0-9_]+`, not starting with `_`, max 64 chars. Queries never create databases — a missing database is `404 database_not_found`.

### The status snapshot

`GET /v1/status` answers five questions: where the gateway is running and
where it keeps its files, how many databases are live and how many are on
their way out, how busy the service is and how often it fails, how long
admission waits are, and which database is doing the work.
The fields below are all additive; the pre-existing ones keep their
meaning.

| Field | Meaning |
| --- | --- |
| `deployment.host` | The machine's hostname, or `null` when the platform does not report one. The client usually cannot work this out for itself: a gateway reached through a tunnel is a loopback port from the outside, and the machine on the other end is the one holding the data. |
| `deployment.listen` | The address the listener **actually bound**, not the one that was requested: `dg serve --listen 127.0.0.1:0` binds an ephemeral port, and a report of the requested address would be a lie. |
| `deployment.data_dir` | The directory the gateway keeps its databases in — the path it was started with, so it is the path in the unit file. |
| `deployment.backup_root` | The backup root the gateway watches, or `null` when none is configured. This is the path, not a claim that anything is backed up; freshness is `GET /v1/backups`. |
| `deployment.disk` | Free and total space of the filesystem the data directory lives on, with the inode counts: `{total_bytes, free_bytes, used_bytes, used_percent, total_inodes, free_inodes}`, or `null` when the platform cannot say. `free_bytes` is the space an unprivileged writer may still use, not the space free of system reservations. |
| `deployment.databases_bytes` | The total the database files occupy. The directory also holds the registry and staging, so this is the databases' own size, not the directory's. |
| `deployment.retention_secs` | How long a deleted name is held before it is purged (86400). |
| `deployment.databases.<db>.file` | The file that database lives in, absolute. |
| `deployment.databases.<db>.exists` | Whether that file is there now. A registry row whose file is gone is a fault, and is reported as missing rather than as a database of zero bytes. |
| `deployment.databases.<db>.size_bytes` | The main file plus its WAL sidecars. |
| `deployment.databases.<db>.tombstoned` | Whether the name is tombstoned: deleted, but still on disk until the retention window purges it. Tombstoned databases are listed here too, because "is everything live" cannot be answered from a list of only the live ones. |
| `database_counts.live` / `.tombstoned` | Live databases, and databases tombstoned but not yet purged. A tombstone still holds its name, so it still counts against the database cap. A client rendering these must treat an absent field as **unknown**, not zero: a gateway old enough to omit `database_counts` would otherwise be drawn as holding no databases while the database list beside it was full. |
| `requests.recent_per_second` | The per-second request counts of the last 60 seconds, oldest first. A fixed-size ring, not a time series: it answers "how busy was it just now" and cannot grow with traffic. |
| `requests.recent_span_secs` | How many seconds the window actually observed. Less than 60 right after boot, so a rate computed from it is scaled honestly rather than reported against a span the service never lived. |
| `requests.wait_p50_us` / `.wait_p95_us` | The median and 95th percentile of the recent **admission waits**, in microseconds — across the whole service. `null` when no wait has been recorded: a percentile of nothing is nothing, and `0` would claim the queue never contended. |
| `requests.wait_samples` | How many waits the percentile sample holds. The sample is a bounded ring of the most recent waits, so it speaks about a recent past, not about the whole uptime. |
| `databases.<db>.queue_wait_p95_us` | The same percentile, but for **that database's own** recent waits. The service-wide percentile mixes every database, so one contended database can hide inside it; this is the figure that names the slow one. `null` when that database has never waited. |
| `databases.<db>.queue_wait_max_us` | The longest single wait that database has ever seen, in microseconds. A different fact from the p95: the p95 says how bad the queue usually is, the max says how bad it ever got. |
| `databases.<db>.queue_waits` / `.queue_wait_ms` / `.queue_timeouts` | Admission attempts for that database, the total time spent waiting, and how many attempts hit the 500 ms bound (`429 capacity_exceeded`). `queue_timeouts` growing under a read flood on one database is the starvation signal. |
| `databases.<db>.last_write` | When the database was last written, as unix seconds, or `null` if it never has been. |

The TypeScript client treats every one of these as optional, so a
server that does not send them still validates.

### The query shapes

`GET /v1/queries` answers the question a hosted provider cannot: **which
statement is the expensive one?** Every statement passes through the
gateway on its way to SQLite, so the gateway is the only place that can
say. For a hosted provider that answer is a privacy liability and a
tenant-isolation problem, and nothing in their business depends on it;
here there is no middleman, and the operator owns the box it all
happens on.

What is recorded is the **shape** of a statement — its SQL with every
literal replaced by a placeholder — and never the values it carried.
`SELECT * FROM users WHERE email = 'ada@example.com'` is kept as
`SELECT * FROM users WHERE email = ?`. A row therefore names a statement
without quoting the data it moved: the log answers "how much does this
cost" without becoming a second copy of the data.

| Field | Meaning |
| --- | --- |
| `shape` | The statement with its literals replaced by `?`. Identifiers and schema survive, because the column name is schema and the schema is what an operator needs to see. |
| `count` | How many times the shape ran. |
| `rows_read` / `rows_written` | The rows it returned and the rows it changed, summed over every run. |
| `total_micros` / `max_micros` | The total time it took, and its slowest single run. The total says what it costs; the max says how bad it ever got. |
| `first_seen` / `last_seen` | When it first and last ran, as unix seconds. |
| `principals` | The credentials that ran it, and how often each did. A credential's **name** is recorded, never its key. |

The endpoint is **owner-only**: the shapes are a view of every
credential's traffic, which is operator information.

Two bounds, both deliberate. A database keeps at most 200 distinct
shapes, and a shape attributes itself to at most 16 credentials; past
either, the least recently seen shape is dropped rather than the log
growing without limit — a query log that fills the disk is the same
failure as any other disk exhaustion. The bound is per database on
purpose: "which query is heavy in THIS database" is the question that
gets asked, and a global cap would let a chatty database hide the
answers for every other one.

Shapes are ordered by rows read, then by total time, then by name, so
the heaviest statement is the first thing read.

The shape log lives in the gateway's memory. It answers a present-tense
question — which statement is heavy *right now* — so a restart costs it
nothing and it starts empty again. The audit trail below is the one
that has to outlive a restart, and it does.

### The audit trail

`GET /v1/audit` answers the question an operator asks after something
unexpected: **which credential did this, and when?** It records ACTIONS
— a credential ran a statement, created a key, deleted a database — and
never the data that moved. A statement is recorded by its **shape**, with
every literal replaced by a placeholder; a credential is recorded by its
**label**, never by its key. The endpoint is owner-only, because the log
is a view of every credential on the box.

The trail is **durable**: it is appended to `<data-dir>/audit.jsonl` as
each entry is recorded, and that file is read back when the gateway
starts. A restart therefore does not erase it — which matters, because a
restart is very often the thing that went down, and a gateway that comes
back up with an empty trail cannot say what its credentials were doing
when it stopped.

The file is the operator's own: mode `0600`, inside the data directory's
`0700`, and it holds no secret — a credential's label and a statement's
shape, never a key and never a value. It is written and flushed as each
entry is made, so a restart of the process keeps it; a host that loses
power can lose the most recent entries, because an `fsync` per request
is not a cost worth paying for a trail that is read after the fact.

The trail is bounded like the log it serves: 2000 entries in memory, and
the file is rewritten from those entries once it passes 4 MiB, so neither
the log nor the file grows without limit.

A backup set does **not** carry the trail. A set carries databases and
the registry, and it is written so that every reader — the producer, the
gateway, the verifier — sees the same shape. The trail is the gateway's
own record of who used those databases, and it stays where it was
written.

### A backup set on demand

`POST /v1/backups/run` takes a backup set NOW, from inside the gateway.
The hourly producer on the host already writes sets into the backup root;
this writes one in exactly the same shape, so the two are
indistinguishable to every reader:

- the set is a directory named `YYYYMMDDTHHMMSSZ`, holding one
  `<database>.sqlite` per live database, a `<database>.sha256` sidecar
  per database, `metadata.sqlite.bak` (the registry) and `manifest.json`;
- each database is copied through SQLite's online backup API, because a
  file copy of a live WAL-mode database is not a valid backup;
- every file is checked BEFORE the set is published — each must be a
  standalone database that passes `PRAGMA integrity_check`, and the
  manifest's size and checksum must agree with the bytes on disk;
- the set is published with an atomic rename, so a reader never sees a
  half-written set;
- the run takes the SAME lock the hourly producer takes
  (`<backup_root>/.backup.lock`), so the two producers exclude each
  other rather than publishing at once. A run that finds the lock held
  fails, and waits for nothing.

The run does not prune: retention is the hourly producer's policy, and
two writers with two retention rules would fight.

### SQL value encoding

Arguments and returned values map JSON ↔ SQLite: `null→NULL`, `boolean→0/1`, `number→INTEGER/REAL` (unsigned integers above `2^63-1` are rejected), `string→TEXT`, `array of 0–255 integers→BLOB` (returned as arrays of numbers). Objects (including `{ "integer": "42" }`) are rejected. Two output coercions are silent by design: non-finite REALs (`NaN`/`±Inf`, unrepresentable in JSON) come back as `null`, and TEXT with invalid UTF-8 bytes comes back lossy (replacement characters). The TypeScript client uses JavaScript numbers: integers outside ±(2^53−1) are not guaranteed exact. For exact large identifiers, store and transmit decimal TEXT; there is no integer-wrapper encoding in M1.

### Errors (stable codes)

| Code | HTTP | Meaning |
| --- | --- | --- |
| `invalid_credential` | 401 | Missing/unknown/revoked key |
| `forbidden` | 403 | App key on a management route |
| `database_forbidden` | 403 | Key not scoped to this database |
| `sql_forbidden` | 403 | Read-only write attempt, or SQL touching protected `_dg_*` tables / disallowed features (attach, pragmas, `ALTER`, triggers/views, temp objects, virtual tables, load_extension, user transactions) |
| `database_not_found` | 404 | No such database (never silently created) |
| `key_not_found` | 404 | Revoking a credential name that does not exist; nothing was revoked |
| `metadata_corrupt` | 500 | Startup migration found an unreadable credential row; the server log names it |
| `internal_error` | 500 | The server could not complete the request for an internal reason (an unreadable snapshot file, a failed write); the server log names it |
| `snapshot_not_found` | 404 | Snapshot unknown, expired, or created by another principal |
| `upload_not_found` | 404 | Upload unknown or expired |
| `invalid_request` | 400 | Malformed JSON, unknown fields, bad name, unsupported arg, empty batch, more than 32 batch statements, SQL text over 64 KiB, more than 256 args, or malformed idempotency key |
| `invalid_request` | 413 | JSON body over the 64 KiB general cap; import requests have a 12 MiB encoded transport envelope, while decoded import data is capped at 8 MiB |
| `invalid_sql` | 400 | Syntax error, wrong parameter count, unknown table/column |
| `result_limit_exceeded` | 413 | Result exceeds 256 KiB or 1000 rows |
| `idempotency_key_conflict` | 409 | Same key, different request body |
| `idempotency_key_not_supported` | 400 | Idempotency key sent on a read-only request; keys only apply to writes |
| `already_deleted` | 409 | Deleting a database that is already tombstoned |
| `database_exists` | 409 | Name already registered/file exists, or tombstoned (delete not yet purged) |
| `already_bootstrapped` | 409 | Second `dg init` attempt |
| `key_exists` | 409 | Credential name reused |
| `file_exists` | 409 | Export destination or clone target already exists on disk |
| `confirmation_required` | 400 | `dg delete` invoked without a matching `--confirm NAME` |
| `capacity_exceeded` | 429 | Global task, per-database queue, credential, or replay capacity full — after a bounded wait, not instantly (see below) |
| `query_deadline_exceeded` | 408 | Execution passed the 2s deadline; transaction rolled back |

Admission waits instead of shedding, on separate read/write lanes. Reads (SELECT-class statements, list/info/export) share 8 read permits and a shared per-database read guard; writes and management mutations take 4 write permits and an exclusive per-database guard. Each wait is bounded by `ACQUIRE_TIMEOUT` (500 ms) before reporting 429. Overload still surfaces as the same `capacity_exceeded` code, only after waiting — so naive clients see far fewer 429s under bursts, but a sustained overload still sheds. Retry 429s with backoff as before. Statement-classified reads also run in a deferred (shared-lock) transaction regardless of credential role, so concurrent reads never shed each other at the SQLite layer; writers keep immediate transactions to fail fast at BEGIN.

### Shared-host isolation limits

One noisy database cannot deny another, but it can delay it. Measured 2026-10-11 with two runs: a same-database flood (`docs/evidence/soak-starvation-2026-10-11.log`, evidence `docs/evidence/soak-starvation-evidence-2026-10-11.json`) and a cross-database flood (`scripts/noisy_neighbor_cross_db.py`, `docs/evidence/noisy-neighbor-cross-db-2026-10-11.log`, evidence `docs/evidence/noisy-neighbor-cross-db-2026-10-11.json`). Box load was high (60-115), so absolute latencies are inflated, but the interleaved control/flood verdict and the flanking-baseline comparison stand.

- **Separate guards per database.** Reads take a shared per-database guard and writes an exclusive per-database guard (`src/store/admission.rs`), so one database's write never blocks another database's reads or writes; only same-database writers serialize. Cross-database proof: while `hot` burned both global lanes (2.9k hot waits, 577 s of hot wait, 24 hot timeouts), all 721 `cold` writes succeeded with zero 429s at 2.3 ms mean wait — the flood hurt only itself.
- **Shared global lanes, bounded delay.** The 8 read / 4 write permits are gateway-wide. Under a 10-reader flood on one database, same-database writer rejections rose from 0.19% (control) to 1.82% (flood, x9.4) — every shed was a per-database `queue_timeout` surfacing as 429 `capacity_exceeded` after at most 500 ms (`ACQUIRE_TIMEOUT`); global lane sheds were zero, transport/internal errors were zero in both arms, and 98%+ of flood-arm writes still succeeded. Degradation, never denial.
- **Writer starvation under read flood is the known residual gap.** Sustained `queue_timeouts` growth in `GET /v1/status` is the trigger for the writer-preferring bound (scale proposal Phase 2); until then a pathological read flood keeps same-database writers waiting in 500 ms slices. The flood also sheds its own readers (167 reader 429s) — it cannot take more than its share without paying in its own rejections.
- **One expensive query is bounded.** Every execution carries a 2 s progress-handler deadline plus a 100k VDBE-op cap (`src/execution.rs`); overrun rolls back with 408 `query_deadline_exceeded` and the database recovers (`deadline_interrupts_expensive_query_and_database_recovers`).
- **No per-database resource partitioning.** CPU and RAM are process-wide: `MemoryMax` bounds the whole gateway, never one database (`docs/resource-budget.md`). A database that grows disk usage affects the shared disk (no per-database quota); disk pressure is drilled in `tests/disk_pressure.rs`.

## Write semantics, Idempotency-Key, retries

Every request runs in one managed transaction; a write is acknowledged only after commit (WAL, `synchronous=FULL`). A failed statement rolls back the whole request. An explicit `query_deadline_exceeded` response means the transaction rolled back. If the transport dies after a write was sent, the outcome is unknown — **never blindly retry an unkeyed write**. To make a write safely replayable, send `Idempotency-Key: <key>`: with the same key and identical body, the server replays the stored original result without re-executing. Key scope is (database, principal, key); retention is 24h, at most 256 keys per principal per database (429 `capacity_exceeded` for new keys at capacity; existing replays still work; other principals unaffected). Read-only credentials must not send the header: a keyed request on a read-only key is rejected with 400 `idempotency_key_not_supported`. Same key + different body = 409. The TS client's `Database.retry(sql,args,key)` is a convenience for explicit replay; resending the original keyed request through the CLI or HTTP API is also supported; no hidden retries exist in client or CLI. Details: `docs/architecture.md`.

## Lifecycle: clone, export, import, delete (M2)

Clones and exports use SQLite's **online backup API**, never raw file copying: the snapshot is a consistent committed point in time even while writes continue on the source (WAL readers do not block writers). Exported files are standalone SQLite databases in DELETE journal mode.

**Import is owner-only.** It validates the file (`PRAGMA integrity_check`, foreign-key check, no protected tables besides the server's own `_dg_replay`) and publishes it in one atomic registry step — a failed import leaves no partial database. The path segment is an informational source label echoed back as `"from"`; only the body's `name` chooses the new database. The CLI sends the same value twice, but raw API callers may label freely — mismatched path/body names are accepted. Import targets must not collide with live **or tombstoned** names. Before publication the gateway ensures its `_dg_replay` schema exists, so **keyed writes and replays work immediately on imported databases**; ordinary external SQLite files need no preparation. A file that already carries a `_dg_replay` table is published only if its definition matches the canonical schema (a smuggled or incompatible variant is rejected), and other `_dg_*` tables are always rejected. Read-write application keys may export within their scope, but cannot publish database imports and cannot clone: a clone names a new database, so clone is owner-only exactly like create and import.

Interruption boundary: publication links the file first and commits the registry row second. A crash between the two leaves an unregistered orphan file. After restart the orphan is invisible (`404`), `create`/import refuse to overwrite or adopt it (`409`), and an unrelated valid import still succeeds; operator recovery is deleting the orphan file and re-importing, which then publishes exact data.

**Delete is owner-only and tombstoned:** the registry row keeps `deleted_at` and a separate `purge_after` deadline. The name immediately disappears from listing and subsequent owner/scoped-key queries return `404 database_not_found`; create/import are refused (`409`) while the tombstone holds. Tombstoned rows also keep counting toward the database cap until purge, so heavy delete/create churn can report `429 capacity_exceeded` until retention expires — this is intentional disk protection, not a leak. Credentials remain valid for their other databases. Already-started requests may finish against the retained file.

The server retains the SQLite file for **24 hours** and checks expired tombstones at startup and every second, even without incoming traffic. Purge waits for request connections to close; failures are logged and retried on later ticks. File removal and registry removal are serialized with publication. The name can be created again after successful purge. Export before deleting anything important: no automated restore is provided until M3.

`dg delete NAME --confirm NAME` requires the exact name to be typed twice. Missing or mismatched confirmation fails locally with `confirmation_required` and sends no DELETE request; `--yes` is not supported. At the HTTP level, sending owner-authenticated `DELETE /v1/databases/{name}` is the explicit destructive request (no body). Lifecycle staging is private per request and automatically cleaned up on both success and error.

Single-shot export/import carries the whole file in one JSON body and is capped at 8 MiB of decoded SQLite data. Larger databases use the **chunked path** (which the CLI uses transparently for all sizes): `export/begin` freezes a consistent snapshot, `GET /v1/snapshots/{id}` fetches it in pieces (default 1 MiB, `offset`/`length` query params), and `DELETE` releases it early. Uploads mirror this: `import/begin`, append-only `chunk` posts at the exact received offset (`GET` status shows it for resume), then `commit` under a target name or `DELETE` to abort. Chunked bounds: 1 MiB per chunk, 128 MiB total per transfer, 8 concurrent transfers, 15-minute retention swept every second. Snapshot ids are creator-bound (the creating principal or the owner); uploads are owner-only. Snapshots and uploads do not survive a server restart — begin again and resume. Commit reuses the validated atomic import above, so the same interruption boundary applies. `dg export --out PATH` verifies the whole-file SHA-256 before its same-directory no-clobber publication: an existing destination is preserved and returns `file_exists`; a successful export is never exposed as a partial file. `dg import` rejects non-regular files and files over the 128 MiB ceiling locally before sending anything.

## CLI

**One rule decides how every command answers.** A terminal gets a human
view; a pipe or `--json` gets JSON. The rule lives in one place, so a
command gets it by asking for a view rather than by remembering it.

**How long a command waits is not the same for a read and a write.** A
read that is given up on costs a retry, so the CLI stops waiting after
ten seconds. A write that is given up on leaves the outcome UNKNOWN —
the operator has to reconcile that by hand, or replay it with the same
idempotency key — so a write waits a minute, and a transfer phase (which
commits nothing, but means beginning the whole transfer again) waits the
same. The answer is what removes the doubt, so the CLI waits for it.

The human views are: `dg list`, `dg info`, `dg inspect`, `dg query`, `dg execute`, `dg status`, `dg watch`, `dg backup status`, `dg backup run`, `dg queries`, `dg audit`, `dg branch list`, `dg config list`, `dg key list` and `dg key tokens` (tables and dashboards), and `dg create`, `dg delete`, `dg export`, `dg import`, `dg clone`, `dg diff`, `dg branch create`, `dg branch delete`, `dg vacuum`, `dg integrity`, `dg explain`, `dg restore plan`, `dg key create`, `dg key revoke`, `dg key rotate-owner`, `dg backup verify`, `dg batch`, `dg config set`, `dg config remove` and `dg init` (a headline and the facts behind it). Piped output always stays JSON, so
scripts are unaffected.

The one exception is `dg backup digest`, which is a REPORT rather than
a command with a machine form: the scheduled unit runs it and its
output lands in the journal, where a human reads it. Its human note is
therefore the default, and `--json` asks for the structured form.

`dg info` also joins per-database `rows_read`/`rows_written` from
`/v1/status` in its table (degraded to `unavailable` when status is
unreachable). Global flags: `--server <url>` (HTTPS or literal loopback
only; defaults to `http://127.0.0.1:7432` when nothing else says
otherwise), `--key-file <path>` (`0600`; optional — see the
connection-resolution order below), `--context <name>` (a named context
from `dg config`), `--json`.

### How the terminal output is drawn

Every human-facing view goes through one renderer (`src/render.rs`), so the
CLI looks the same everywhere. The rules it follows:

- **The machine surface is untouched.** Piped stdout and `--json` stay
  byte-identical JSON. Decoration happens only on the terminal path, and
  a table border or an escape sequence never reaches a pipeline.
- **Layout is measured, not counted.** Column widths come from the
  display width of the text, so a CJK value or an emoji in a cell does not
  shear the grid.
- **Numbers are right-aligned** under their header when a column's values
  are all numeric; a column holding any text stays left-aligned. A SQL
  `NULL` renders as a dim `NULL`, distinct from the text `"NULL"`. A row
  shorter than the header renders empty cells rather than claiming a
  `NULL` that was never sent.
- **Values are humanised where a human reads them**: byte counts as
  `16.0 KiB`, durations as `1h 30m`, epoch timestamps as
  `2026-10-19 23:15 UTC` (with the raw epoch kept alongside it), and
  backup-set stamps as `2026-10-19 23:15`. The machine forms are still
  in the JSON output.
- **Colour is decoration, never information.** A state that is coloured
  (`fresh`, `stale`, `unknown`, a non-zero error counter) is also spelled
  out in words, so the output reads the same without colour. `NO_COLOR`
  and a `TERM=dumb` terminal turn it off; a non-UTF-8 locale gets an
  ASCII grid instead of box-drawing characters.
- **A cell's contents cannot repaint the terminal.** Control characters
  in a value are escaped visibly (``), because a cell's contents come
  from the wire over a connection the CLI accepts on any origin. The
  renderer owns this rule: it applies to every cell, column header,
  table footer and section title, whether the value came from a query
  row, a database name, a key name, a service version or a backup
  state, so no view can be the one that forgets it.
- **`dg status` and `dg watch` are dashboards**, grouped into
  titled sections (service, activity, admission, per-database) rather
  than one long `property | value` list. They answer the questions an
  operator has — how many databases are live and how many are
  tombstoned, how busy the service is and how fast, how often it
  fails, and which database is doing the work — rather than listing
  the server's properties. `dg watch` is the same dashboard read once:
  it exits rather than refreshing in place, so it stays pipeable and
  diffable.
- **`dg inspect <db>` is one database**: what it holds AND what it has
  been doing. `dg info` is what a database IS; `dg inspect` is what it
  has been DOING.
- **The shell is the control surface.** `dg shell` has dot-commands for
  the operator's whole surface — `.status`, `.keys`, `.backups`, plus
  `.tables` and `.schema` — and they draw the SAME views as the
  one-shot commands, so the two cannot drift apart.

Connection resolution, highest precedence first: `--server` / `--key-file` flags, then `--context <name>`, then `DG_SERVER` / `DG_KEY_FILE` environment variables, then the config file's default context, then the built-in defaults (`http://127.0.0.1:7432` and `$HOME/.config/dg/key`).

A **context** is a named pair of a server and a credential FILE, kept in `$HOME/.config/dg/config.toml` (created `0600`, in a `0700` directory). Nothing secret is ever written down: the file holds paths, never a key. The context sits between the flag and the environment on purpose — the environment says "where do I usually talk", a context says "talk to THIS one now", and the flag is the operator's word at the command line, which outranks both. A `--context` that names a context the file does not define is an error, not a silent fallback: quietly using another gateway is worse than refusing.

With the env vars set — or the default key file in place — daily commands need no flags at all: `dg list`, `dg query game_a "SELECT …"`.

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
printf '%s\n' "SELECT count(*) FROM players;" > script.sql
dg --key-file owner.key execute game_a script.sql
dg --key-file owner.key inspect game_a
dg --key-file owner.key queries                 # which statement is the expensive one
dg --key-file owner.key queries --database game_a
dg --key-file owner.key audit --principal bot_a # what did that credential do
dg --key-file owner.key clone game_a game_b
dg --key-file owner.key diff game_a game_b      # what differs between two databases
dg --key-file owner.key branch create game_a game_a_trial
dg --key-file owner.key integrity game_a        # is the file sound
dg --key-file owner.key explain game_a "SELECT * FROM players"
dg --key-file owner.key restore plan --to 2026-10-16T00:00:00Z
dg --key-file owner.key watch
dg --key-file owner.key key tokens game_a   # the credentials that can act on one database
dg --key-file owner.key key revoke bot_a
```

```sh
dg config list                       # the contexts, and whether their key file is there
dg config set staging --server http://127.0.0.1:7432 --key-file ~/.config/dg/staging.key --default
dg config show staging
dg --context staging list            # talk to that one now
dg config path                       # where the file is
dg config remove staging
```

Running a SQL file (the statements are split by SQLite itself, so a
semicolon inside a string or a trigger body is not a boundary, and the
whole file runs as one atomic batch):

```sh
dg --key-file owner.key execute game_a migrations.sql
```

The batch API accepts 32 statements at a time, and the whole file is one
atomic batch, so a file of more than 32 statements is refused before
it is sent: the command reports how many statements the file holds,
what the API accepts, and that nothing was sent. A longer file cannot
be run by splitting it into several batches, because that gives up the
atomicity a migration needs. `dg batch` enforces the same cap on a
JSON batch file.

Lifecycle commands (M2):

```sh
dg --key-file owner.key clone game_a game_a_copy
dg --key-file owner.key export game_a --out ./game_a_snapshot.sqlite   # writes the file, prints {name,out,size,sha256}
dg --key-file owner.key import restored --from ./game_a_snapshot.sqlite
dg --key-file owner.key delete game_a_copy --confirm game_a_copy   # tombstone; file purge after 24h retention
```

Operator visibility (owner-only; the AI-operator surface — poll these, not log files):

```sh
dg --key-file owner.key status          # the service dashboard: counts, rate, error rate, per-db table
dg --key-file owner.key watch           # the same dashboard, read once (it exits, so it stays pipeable)
dg --key-file owner.key inspect game_a  # one database: what it holds and what it has been doing
dg --key-file owner.key backup status   # versioned sets plus per-database fresh/stale/unknown
dg backup digest                        # the delivered daily digest, computed from the data dir (no server needed)
dg backup verify --backup-root <dir> latest   # re-verify a published set offline (no server needed)
dg --key-file owner.key tui              # the terminal view: the service, its databases, their backups and its keys, live
```

Versioned sets are produced by `scripts/micro2-backup.sh` (one run at a time per backup root; stale staging from killed runs is cleaned at startup; every run appends an outcome line to `<backup-root>/attempts.jsonl`). The script exports each live database through the online-backup path, copies the registry through the SQLite backup API, validates hashes, integrity, foreign keys, and protected tables before atomic publication, then prunes to `--keep` sets. `dg backup verify` repeats the manifest/hash/integrity/FK/protected-table checks on demand and prints a JSON report (`ok`, `failures`, member list); it exits nonzero with `backup_verification_failed` when any check fails, or `backup_set_not_found` for unknown sets. Restore follows [the recovery contract](recovery-contract.md) §3 (registry-restored or fresh-registry path).

`dg tui` is the terminal operator view: the same six answers — the service dashboard, the databases, the backup freshness, the credentials, the query shapes and where the database files live — on one screen, read again every two seconds, with the lifecycle, credential and SQL actions a keystroke away ([the view's own guide](tui.md)). Each tab answers one question and shows one answer, so a wide table is never squeezed into a fraction of the width. It asks for the same routes this document describes and changes none of them, so the API a client sees is the API it saw before. The credential never reaches the screen: the view holds it in memory, and the only key it ever draws is a newly issued one, which the gateway shows once and never again.

`GET /health/live` is unauthenticated liveness only (`{"status":"ok"}`) — it says nothing about database, disk, or backup readiness. `/v1/status` counters are status-level (total plus 429/408/500 signals); per-code detail lives in the structured logs. The per-database map adds the admission-queue measurement (`queue_waits`, `queue_wait_ms`, `queue_timeouts`): sustained `queue_timeouts` growth means per-database writer starvation under read flood and is the trigger for the writer-preferring bound (scale proposal Phase 2). `/v1/backups` reads versioned sets from the backup root (`<data-dir>/../dg-backups` convention, `--backup-root` override; missing root means `unknown`, never healthy). Per-database state compares the last successful API mutation against the newest set containing the database: `fresh` (set covers all writes, or no writes recorded), `behind` (writes newer than the newest set, but the change has been unrepresented for no longer than the approved snapshot cadence — the producer is between publications), `stale` (an unrepresented change has waited longer than that cadence), `unknown` (no root, unreadable root, or no set contains it). `lag_secs` says how long the newest unrepresented change has waited, and is present only when there is one. The cadence is the owner's number, set with `dg serve --backup-cadence-secs` and `dg backup digest --cadence-secs`; with no cadence approved there is no `behind` and every unrepresented change is reported, which is the strict reading. `stale_after_secs` (300) is the look-at-it reference from the observability contract, not an accepted RPO. Freshness is set membership by timestamp, not a restore drill — restore readiness stays a separate rehearsal. `dg serve --help` lists the `--backup-root` flag. Per-database freshness is a claim about the LIVE databases: a database that has been deleted is not stale, it is gone, so it is counted in `deleted` rather than listed in `per_database` and never asks for the operator's attention.

`GET /v1/backups` also answers the questions that decide whether the copies can be trusted, and each has its own block, because they are different claims:

| Block | Meaning |
| --- | --- |
| `last_attempt` | The last backup run: `{ts, started_at, completed_at, outcome, set, databases, step}`. This is the only durable evidence of a run that **failed**: a failed run publishes no set, so "no new set" cannot tell a failure from a job that never ran. `last_failure` is the newest failed attempt, which is a different record when a later run succeeded. |
| `coverage` | What the newest published set carries: `{live, in_newest_set, missing, registry}`. `registry` is the one that matters most: without the registry a set holds the data but not the credentials and scopes that make it usable, so a restore from it gives back files that no key can open. |
| `replica` | Where the off-host copy is, as the host publishes it: `{target, bucket, endpoint, region, retention, snapshot_interval, databases, registry, sidecar, published_at}`. The gateway cannot know this by itself — the sidecar's configuration is the host's business — so the host publishes it into the backup root (`scripts/litestream-sync.sh` writes `replica.json`) and the gateway reads it the way it reads the attempt log. `sidecar` is whether the thing that does the copying is running: a configured replica that is not replicating is the failure that looks most like success from a distance. The credentials are never in this file: the fields are a whitelist, so a bucket, an endpoint and a region are what can be reported. |
| `drill` | The last off-host restore drill: `{drilled_at, outcome, databases, detail}`. A backup that was never restored is an assumption; a drill that restored the copies and compared them against the live gateway is evidence. `scripts/bucket-restore-drill.sh` publishes it as `restore-drill.json`, whether the drill passed, failed, or was cut short. |
| `restore_points` | The window of moments the off-host replica can be taken back to: `{published_at, window_start, window_end, sidecar, databases}`. The replica keeps a rolling window of write-ahead log, so the data can be restored to a moment and not only to the last snapshot. How wide that window is belongs to the host — the bucket and the sidecar are its — so `scripts/restore-point.sh --list` publishes it as `restore-points.json` and the gateway reads it. `databases` is per database: `{oldest, newest, files, bytes}`. `oldest` is the oldest file the replica holds for that database — the snapshot and the compacted write-ahead log, which is what covers the time between snapshots. `newest` is the moment the replica is CURRENT to, not the last time it happened to write a file: a database that has not changed since its snapshot can still be restored to any later moment, because the bytes are the same ones the replica holds now. When the sidecar is not running the state is frozen, and `newest` is the newest file it holds. |
| `last_restore` | The last point-in-time restore taken from the replica: `{restored_at, database, timestamp, outcome, detail, tables, rows, size_bytes, integrity, path}`. A window nobody has ever exercised is a promise; a restore that was taken, verified and published is evidence. `scripts/restore-point.sh --apply` publishes it as `restore-results.json`, whether the restore succeeded, failed, or was cut short. |

A block the host has not published is `null`, and a client must draw that as *not reported* — never as "there is no replica", "the drill never ran", "the last run worked", or "there is nothing to restore from". The difference is the whole point: an absent fact and a false fact are not the same fact.

Freshness is computed once, in `src/backup_health.rs`, and shared by `GET /v1/backups`, `dg backup status`, `dg backup digest` and the notification sink, so the digest an operator receives cannot disagree with the endpoint they check against. `dg backup digest` reads the data directory and backup root directly rather than asking the gateway, so it works while the gateway is stopped; it prints the delivered note (`--json` for the structured form) and exits nonzero with `backup_unhealthy` when any database is stale beyond the approved cadence or unknown — a digest that exited 0 over a change the backup has not taken is how a schedule gets mistaken for a backup. A change that is merely `behind` does not fail it: with an hourly producer every change waits for the next publication, and failing on that made the daily digest red for an hour after every write, which is an alarm that says nothing about whether the backup works.

`dg serve` can also *deliver* backup health instead of waiting to be asked, which is the owner's 2026-09-27 decision ([observability contract](observability-contract.md) §3a): `--backup-digest-secs N` (daily by default, off unless set) sends the per-database digest, `--backup-failure-poll-secs N` sends an immediate notice the first time each failed attempt appears in `attempts.jsonl`, and `--backup-notify-webhook URL` adds an HTTP POST of the same note (HTTPS or literal loopback only) alongside the structured log line. A repeated failure is announced once, not once per poll. Delivery carries state, the newest verified set, an age, the failing step and a bounded action — never an RPO, an RTO, or a data-loss claim.

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

**No retries occur.** `:key` applies to the next SQL/batch attempt. It is consumed after a completed request, including API and local errors; a transport failure reported as `outcome_unknown` retains the key so the next identical SQL/batch line can be an explicit replay. `.tables`/`.schema` do not consume it. Cancelling a batch or switching database clears it. To explicitly replay a write after `outcome_unknown`, submit the identical body with the retained key (or set the same key again) within the 24h retention window. Never blindly repeat an unkeyed write. The shell neither writes credentials nor saves SQL history to disk.
