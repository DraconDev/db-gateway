# Architecture and decision register

**Document role:** Current product and decision register
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)

## Agreed product boundary

DB Gateway (`dg`) serves one owner and database-scoped applications. The server owns validation, authorization, SQL execution, and lifecycle safety; the CLI calls that server rather than implementing separate database behavior.

Target layout:

```text
Owner CLI / application clients
             | HTTPS or private transport
             v
     Rust gateway on deployment-host
       | independent SQLite files on persistent disk
       v
     SQLite-aware backup component
       | asynchronous upload
       v
     object storage
```

The initial host has approximately 1 GB RAM. A single host is not high availability. Local commit durability does not imply that the latest write has reached the backup bucket.

Non-goals: billing, organizations, SSO, general enterprise roles, a hosted/public web UI, automatic failover, transparent SQLite/Postgres interchangeability, and guaranteed zero-loss host recovery. A workstation-local view is allowed as a local companion to the CLI
(the terminal view, `dg tui`). Later independent-database placement and ephemeral storage are not M0/M1 requirements.

## M0 implementation decisions (historical — superseded)

> This section records the M0 foundation as it stood then. It is **not**
> the current system: today the gateway serves the full authenticated
> API in [`api.md`](api.md) (SQL, lifecycle, keys, backup health), and
> M1–M3 are delivered — see [`TASKS.md`](../TASKS.md) for the current
> status ledger. The decisions below still explain the foundation's
> shape (single workspace, local-only, structured logs).

- One Rust package in a workspace exposes a `dg` executable and testable library.
- Clap parses commands; Tokio runs the network runtime; Axum supplies routing.
- The only route is `GET /health/live`, returning HTTP 200 and `{"status":"ok"}`. Other routes are absent.
- Listening is restricted to local while authentication is absent. No database files, keys, or configuration files are created.
- Structured operational logs go to stderr. CLI parse errors remain Clap's normal usage errors.
- Library/router and binary/argument handling are separated now. Introduce execution, lifecycle, and credential modules with their implementations; empty placeholder modules would not establish real boundaries.

This foundation chooses neither a SQL engine binding nor a client wire protocol.

## Required decisions and evidence

| Decision | Status | Required before |
| --- | --- | --- |
| Custom HTTP client versus required libSQL compatibility | **Decided (M1): custom JSON HTTP API** — smaller implementation surface for the first client; libSQL compatibility is a later layer if migration demands it | Resolved |
| First real client | **Decided (M1): TypeScript** (`client/`, `@dracon/db-gateway`) — the owner's bots/games are TypeScript, so the first real integration test must exercise the language the applications actually use; a libSQL-compat layer can be added later if migration demands it | Resolved |
| SQL values, integer/blob encoding, error codes and route schemas | **Decided (M1)**: JSON null/bool/number/string/byte-array blobs; stable error codes; `columns`/`rows`/`rows_affected` | Resolved for M1 |
| Single statements, atomic batches, deadlines and ambiguous writes | **Decided (M1)**: all SQL in managed transactions, 2s deadline with progress-handler interruption, rollback on error | Resolved for M1 |
| Idempotency support and retention/resource bounds | **Decided (M1)**: `Idempotency-Key` header, periodic retention, 256 keys per principal per database, replay in the same transaction as the write prevents double execution after a lost acknowledgement; no unkeyed write retry | Resolved for M1 |
| Recovery objectives (recovery objectives) | **Working target accepted 2026-09-27 (Profile C): periodic recovery objective, periodic recovery objective, extended frequent / 30 periodic production / 7 periodic development versions (`recovery-decisions.md` §4, §6.1)** — a target, not a guarantee; production acceptance is still pending | Production acceptance |
| object storage provider compatibility, tool/version, retention and registration lifecycle | **Verified 2026-10-14: 12/12 sandbox gates PASS against a live object storage bucket** (replication sidecar 0.5.17 pinned; `evidence/sandbox-2026-10-14-object storage provider.log`); sidecar live on deployment-host since 2026-10-15 with automatic new-database coverage since 2026-10-19 (`evidence/replication-sync-drill-2026-10-19.log`). Retention canonicalization across the two backup tiers remains open (audit A5) | Resolved for M3 |
| SQLite concurrency, connection/cache/queue/result limits | **Decided 2026-10-15 (G1 triage): keep READ_PERMITS=8, WRITE_PERMITS=4, ACQUIRE_TIMEOUT=500ms with the ticket-lock AdmissionQueue (src/store/admission.rs)** — the 2026-10-15 e2e MARGINAL (writer reject 0.015%->0.040% under 10-reader flood) did not reproduce: rerun verdict NO_STARVATION (0.06%->0.00%, 0 flood writer rejections in 6798 attempts, 14451 flood reads served). The ticket lock (admission by ticket order, writer admitted at next reader batch boundary) is the durable fix for the earlier 7.8x polled-RwLock starvation and the sideways writer-preferring flag; absolute reject rates stay <0.1% in both arms, so no permit change is warranted. Revisit only if a future run shows STARVED. | M1 limits; M4 release gate |
| Host CPU architecture, disk and access transport | **Verified**: Ubuntu x86_64, 2 vCPU, glibc 2.39, persistent `/var/lib/dg`; access is local + private transport by decision (no gateway TLS) — `evidence/first-deploy.md`, `deployment-readiness.md` gates 1–3 | Resolved |

## M1 design constraints

- Explicit creation: missing databases must never be silently created by a query.
- Owner administration and separate revocable read-only/read/write scoped keys; each scope is bound to a durable database generation so purged-name replacements cannot inherit old credentials; no SQL endpoint before authorization is in place.
- SQLite controls enforce scope and read-only permissions; SQL prefix matching is insufficient.
- Blocking execution stays off async network threads, with bounded admission, results, and real query cancellation.
- Atomic batches roll back on failure and return clean connections. Cross-database atomicity is not promised.
- A timeout or lost response can leave a committed write's outcome unknown; document this before shipping writes.
- Clone/export/restore use consistent SQLite-aware operations and coordinated lifecycle handling.
- Recovery claims require verified restores and visible backup failures, not merely a configured bucket.
- The first production bucket profile is per-database consistent verified snapshots, not a cross-database atomic snapshot; registry/file membership is reconciled during restore.
- The operator's surface is the terminal: the one-shot CLI, the interactive shell, and the full-screen view (`dg tui`), which read their facts from one shared view layer so they cannot disagree about the same gateway. The view holds the credential in memory and never draws it, except for a newly issued key, which the gateway shows once.

See `TASKS.md` for executable milestones and acceptance conditions. No task completion authorizes deployment, production data changes, or subscription changes.

## M1 write semantics (implemented)

Every application SQL request runs in one managed transaction on a fresh connection. A write is acknowledged only after its transaction commits (`synchronous=FULL`, WAL). On statement failure the transaction rolls back and the connection is dropped.

Ambiguous outcomes: if the transport fails after a write is sent, the client reports `outcome_unknown`; the committed state is then unknown. An explicit HTTP 408 `query_deadline_exceeded` response instead means execution was interrupted and the transaction rolled back. The documented client policy is: **never blindly retry an unkeyed write**; to make a write safely replayable, send an `Idempotency-Key` header. With the same key and identical request body, the server replays the stored original result without re-executing. Key scope is (database, principal, key): the same key string under a different principal or on a different database is an independent slot, never a collision. Retention is 24 hours with at most 256 stored keys **per principal per database**; a principal at capacity receives 429 `capacity_exceeded` for new keys while replays of existing keys keep working, and other principals are unaffected (regression-tested). A reused key with a different body is a 409 `idempotency_key_conflict`. Replay records commit atomically with the write itself, so a crash cannot separate them. The TypeScript client's `Database.retry(sql,args,key)` is the explicit caller-initiated replay path; no hidden automatic retry exists in the client or CLI.

Owner bootstrap is local-only (`dg init --data-dir …`), not an HTTP endpoint: the owner credential is generated locally, printed once, and stored only as a SHA-256 hash. Issued app keys are revocable, database-scoped read-only/read-write credentials whose stored scope includes the database generation; read-only enforcement uses SQLite's authorizer plus statement readonly checks, never SQL prefix matching. `_dg_*` tables are protected from application SQL. Listening is local-only in M1; both Rust and TypeScript clients refuse non-HTTPS, non-local endpoints.
