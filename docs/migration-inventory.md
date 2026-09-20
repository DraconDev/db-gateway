# Turso/libSQL parity inventory + migration checklist

Status: **preparation only**. No Turso account, database, or application
repository was accessed. Compatibility notes below are a dated, public-source
reference inventory of the concerns that affect this gateway; they are not a
claim to reproduce Turso's entire product catalog. Anything marked UNVERIFIED
must be confirmed against current Turso docs or a disposable test database
before any migration plan.

The design intent is to use Turso/libSQL as a reference **mold**, not as a
compatibility target: keep the gateway's custom JSON protocol, single-owner
boundary, hash-only credentials, explicit lifecycle, and bounded local
SQLite semantics unless a later owner-approved requirement says otherwise.

## Compatibility risks (source-backed, repo-grounded)

| # | Area | Risk | Marker |
|---|---|---|---|
| 1 | Client/API | The gateway speaks a custom JSON API (`POST /v1/databases/{name}/query`, bearer auth, `Idempotency-Key` replay). Turso speaks Hrana (WebSocket/HTTP) and libSQL client protocols. Every caller (CLI, TS client, shell) must be rewritten or re-pointed; nothing is wire-compatible. | REPO-VERIFIED (our side, `docs/api.md`) / UNVERIFIED (exact Turso endpoint shape) |
| 2 | Auth model | Gateway credentials are 64-hex bearer keys hashed server-side with per-database roles. Turso uses platform-issued auth tokens per database. All keys must be replaced; there is no key import path. | REPO-VERIFIED / UNVERIFIED |
| 3 | `_dg_replay` table | Every gateway database file contains the server-owned `_dg_replay` table. It must be dropped (or the schema re-dumped without `_dg_*` tables) before importing anywhere else; carrying it over pollutes the target and may collide with future tooling. | REPO-VERIFIED |
| 4 | SQL dialect | Gateway executes stock SQLite via rusqlite with an allowlist (`SELECT/INSERT/UPDATE/DELETE`, `sql_forbidden` otherwise). libSQL is a SQLite fork: standard DDL/DML transfers, but any libSQL-only syntax has no gateway equivalent and any gateway-allowed edge (e.g. `RETURNING`, triggers) needs per-statement testing on Turso. | REPO-VERIFIED / UNVERIFIED |
| 5 | Value limits | Gateway caps: 256 args/statement, 64 KiB SQL text, 256 KiB or 1000-row results, 8 MiB export. Turso/Hrana has its own request/response limits — workloads near our caps must be re-tested there, not assumed portable. | REPO-VERIFIED / UNVERIFIED |
| 6 | Sizes | Database/schema/row counts must fit the target plan. No Turso size or row limits are quoted here — plans change; check current Turso docs at migration time. | UNVERIFIED |
| 7 | Writers | Gateway: single writer per database, 1-writer semaphore, 2 s deadline, 429 backpressure. Turso: primary-writer with read replicas (topology differs). Applications relying on 429/408 retry behavior must be re-tuned. | REPO-VERIFIED / UNVERIFIED |
| 8 | Idempotency | Gateway `Idempotency-Key` replay (24 h, 256 keys) has no Turso equivalent. Exactly-once resubmission logic in callers must be redesigned (client-side dedupe or transactional outbox). | REPO-VERIFIED |
| 9 | Lifecycle | Tombstone + 24 h purge, clone/export/import routes, and `_dg_*` protection are gateway concepts with no Turso counterpart. Deletion becomes a platform operation with its own retention story. | REPO-VERIFIED |
| 10 | Recovery | Gateway recovery = versioned export snapshots + documented restore (`docs/recovery-contract.md`). Turso point-in-time recovery is plan-dependent and UNVERIFIED here; until confirmed, gateway exports remain the portable fallback and must continue during any transition. | REPO-VERIFIED / UNVERIFIED |
| 11 | Latency/topology | Gateway is loopback-only with ~4 ms local p50 (`docs/resource-harness.md`). Turso is network-attached; expect different latency, TLS, and failure modes (timeouts, reconnects) that local tests never exercised. | REPO-VERIFIED / UNVERIFIED |

## Design response: borrow the mold, keep the gateway boundary

| Reference concern | Keep from the comparison | Adapt or reject for DB Gateway | Evidence needed before changing course |
| --- | --- | --- | --- |
| Client protocol | A typed client and explicit request/response contract | Keep the custom JSON HTTP API; do not add Hrana/libSQL wire compatibility by implication | A separately approved application requirement plus a versioned protocol design |
| Database engine | SQLite data and SQL portability where statements are standard | Keep stock SQLite/rusqlite and test statement-by-statement; reject libSQL-only syntax unless explicitly designed | A disposable parity fixture with exact rows, errors, and limits |
| Authentication | Database-scoped application access is a useful boundary | Keep hash-only owner/app credentials and generation-bound scopes; do not import platform tokens or SaaS tenancy | A new security review and migration inventory for any auth change |
| Replication/topology | Off-host recovery and observable replica freshness are useful goals | Do not assume remote replicas, edge placement, or high availability; evaluate Litestream/OVH independently under `docs/backup-compatibility.md` | Failure, latency, resource, and restore evidence on the chosen target |
| Lifecycle/control plane | Explicit create, export, restore, and deletion operations need operator intent | Keep tombstones, validated import, and owner-only management; do not inherit opaque platform lifecycle semantics | An owner-approved lifecycle contract and regression suite |
| Limits/retries | Clients need bounded requests and explicit retry behavior | Keep gateway limits and keyed replay; never copy plan limits, hidden retries, or availability claims from a hosted service | Current vendor documentation plus workload measurements |
| Recovery/observability | Freshness, restore verification, and failure visibility are first-class concerns | Implement the local signal contract in `docs/observability-contract.md`; keep RPO/RTO/retention undecided until `docs/recovery-decisions.md` is accepted | Passing sandbox restore/failure/resource matrices and owner approval |
| Commercial scope | Vendor documentation can expose useful operational patterns | Reject billing, SSO, organizations, and SaaS tenancy from the single-owner product boundary | Explicit product-scope change, not migration pressure |

This comparison is intentionally decision-oriented: it records what to adopt,
adapt, reject, and verify instead of turning a vendor feature list into an
unbounded compatibility promise. Public reference entry points are
[docs.turso.tech](https://docs.turso.tech/) and the
[libSQL project](https://github.com/tursodatabase/libsql); every future vendor
claim must carry its access date and an `UNVERIFIED` marker until checked.

## Operator-fillable inventory (empty — fill during survey)

Copy per database:

```
- database name:            (gateway name)
- file size (bytes):        (du -b <name>.sqlite)
- tables/rows:              (SELECT name FROM sqlite_master; per-table counts)
- app writers:              (which applications write, how often)
- app readers:              (which applications read, latency needs)
- max statement args:       (audit against the 256 cap)
- max result rows:          (audit against the 1000-row cap)
- idempotency keys in use:  (which callers send Idempotency-Key)
- credentials to replace:   (key names + roles from `dg key` listing)
- _dg_* tables present:     (must list _dg_replay at minimum)
- recovery requirement:     (RPO/RTO per docs/recovery-decisions.md)
- Turso target group/plan:  (UNVERIFIED until chosen)
```

## Local fixture rehearsal and its limits

`tests/migration_fixture.rs` imports a synthetic external SQLite file
(authors/books tables, two indexes, a foreign key, INTEGER/REAL/TEXT/BLOB/
NULL/JSON values) through the existing import API, verifies exact restored
rows plus surviving indexes, and proves a fixture smuggling a `_dg_shadow`
table is rejected (`invalid_request`) with nothing published. Limits: the
fixture is synthetic and tiny; it exercises the gateway import path only.
It says nothing about Turso's dialect, limits, auth, latency, or import
behavior, and must never be cited as migration evidence.

## Unsupported assumptions (do not assume)

- That Turso accepts a raw gateway `.sqlite` file byte-for-byte (WAL mode,
  `_dg_replay`, and page-size details need a disposable trial import).
- That any current Turso pricing, plan limit, or feature list in memory or
  in older notes is still accurate.
- That the TS client (`client/src/index.ts`) ports by changing a base URL —
  the protocols differ completely (risk 1).
- That migration can proceed before the deployment-readiness gates
  (`docs/deployment-readiness.md`) are resolved — migration presumes a
  running source of truth with backups.
