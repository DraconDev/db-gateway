# Turso → gateway migration inventory + checklist

Status: **preparation only**. No Turso account, database, or application
repository was accessed. The gateway is the migration **target**: Turso
databases move into it, and Turso is decommissioned only after a successful
soak (see `docs/migration-cutover.md`). Notes below are a dated,
public-source reference inventory of the concerns that affect bringing a
Turso database under this gateway; they are not a claim to reproduce Turso's
entire product catalog. Anything marked UNVERIFIED must be confirmed against
current Turso docs or a disposable trial export before any migration plan.

The design intent stands: keep the gateway's custom JSON protocol,
single-owner boundary, hash-only credentials, explicit lifecycle, and bounded
local SQLite semantics. Migration adapts the **source data and callers** to
the gateway; it does not add Turso wire compatibility to the gateway.

## Compatibility risks (source-backed, repo-grounded)

The source of truth in each row is either this repo (REPO-VERIFIED: the
gateway side) or Turso (UNVERIFIED until checked against current docs or a
disposable export).

| # | Area | Risk | Marker |
|---|---|---|---|
| 1 | Client/API | Applications speak Turso (Hrana/libSQL clients). The gateway speaks a custom JSON API (`POST /v1/databases/{name}/query`, Bearer keys, `Idempotency-Key` replay). Every caller must be re-pointed at the TS client, CLI, or raw HTTP; nothing is wire-compatible. | REPO-VERIFIED (our side, `docs/api.md`) / UNVERIFIED (exact source endpoint shape) |
| 2 | Auth model | Turso platform-issued auth tokens must be replaced with gateway 64-hex Bearer keys (owner + per-database read-only/read-write roles, generation-bound scopes). All keys are replaced; there is no token import path. | REPO-VERIFIED / UNVERIFIED |
| 3 | Denied SQL (highest risk) | The gateway authorizer denies views, triggers, `ALTER`, temp objects, virtual tables, pragmas, and user transactions (`403 sql_forbidden`). Source schemas using any of these must be flattened or redesigned before import, and every statement re-tested — a Turso export carrying a view or trigger cannot run here. | REPO-VERIFIED (`src/execution.rs`, `tests/schema_shapes.rs`) |
| 4 | SQL dialect | libSQL is a SQLite fork: standard DDL/DML transfers, but any libSQL-only syntax has no gateway equivalent and must be rewritten. | REPO-VERIFIED (stock SQLite via rusqlite) / UNVERIFIED (source feature census) |
| 5 | Value limits | Gateway caps: 256 args/statement, 64 KiB SQL text, 256 KiB or 1000-row results, 8 MiB single-shot JSON body, 128 MiB chunked transfer. The single-shot `/v1/databases/{name}/import` route refuses above 8 MiB, but `dg import` always uses the chunked path, so the single-shot limit is not an import ceiling. Source workloads near or over the chunked ceiling must be reshaped and re-tested, not assumed portable. | REPO-VERIFIED (`docs/api.md`) |
| 6 | Sizes | Every source database file must fit the 128 MiB chunked transfer ceiling (`MAX_TRANSFER_SIZE`) or it cannot enter the gateway by the supported path (see finding F2 in `docs/audit-2026-09-21.md`); the 8 MiB single-shot body limit does not apply to `dg import`. Record exact bytes plus growth headroom per database. | REPO-VERIFIED (cap) / UNVERIFIED (source sizes) |
| 7 | Writers | Turso primary/replica topology becomes gateway single-writer-per-database with 429/408 backpressure. Applications must re-tune retry behavior to gateway semantics. | REPO-VERIFIED / UNVERIFIED (source topology) |
| 8 | Idempotency | The gateway offers `Idempotency-Key` replay (24 h, 256 keys) that Turso callers lack. Migrated writers should adopt keyed writes for exactly-once resubmission instead of blind retries. | REPO-VERIFIED |
| 9 | Lifecycle | Tombstone + 24 h purge, clone/export/import routes, and `_dg_*` protection are gateway concepts operators must learn; Turso platform operations no longer apply after cutover. | REPO-VERIFIED |
| 10 | Recovery | Gateway recovery = versioned export snapshots + documented restore (`docs/recovery-contract.md`). Turso point-in-time recovery is left behind: gateway exports must be running before cutover, and Turso-side exports are retained as the rollback floor through the soak. | REPO-VERIFIED / UNVERIFIED (source export mechanics) |
| 11 | Latency/topology | Network-attached Turso becomes loopback gateway (~4 ms local p50, `docs/resource-harness.md`). Expect lower latency but different failure modes: no reconnects, explicit 429/408 backpressure instead. | REPO-VERIFIED / UNVERIFIED |

## Design response: keep the gateway boundary

| Reference concern | Keep from the comparison | Adapt or reject for DB Gateway | Evidence needed before changing course |
| --- | --- | --- | --- |
| Client protocol | A typed client and explicit request/response contract | Keep the custom JSON HTTP API; do not add Hrana/libSQL wire compatibility by implication | A separately approved application requirement plus a versioned protocol design |
| Database engine | SQLite data and SQL portability where statements are standard | Keep stock SQLite/rusqlite and test statement-by-statement; reject libSQL-only syntax unless explicitly designed | A disposable parity fixture with exact rows, errors, and limits |
| Authentication | Database-scoped application access is a useful boundary | Keep hash-only owner/app credentials and generation-bound scopes; do not import platform tokens or SaaS tenancy | A new security review and migration inventory for any auth change |
| Replication/topology | Off-host recovery and observable replica freshness are useful goals | Do not assume remote replicas, edge placement, or high availability; evaluate Litestream/OVH independently under `docs/backup-compatibility.md` | Failure, latency, resource, and restore evidence on the chosen target |
| Lifecycle/control plane | Explicit create, export, restore, and deletion operations need operator intent | Keep tombstones, validated import, and owner-only management; do not inherit opaque platform lifecycle semantics | An owner-approved lifecycle contract and regression suite |
| Limits/retries | Clients need bounded requests and explicit retry behavior | Keep gateway limits and keyed replay; reshape sources that exceed them instead of raising caps by migration pressure | Current vendor documentation plus workload measurements |
| Recovery/observability | Freshness, restore verification, and failure visibility are first-class concerns | Implement the local signal contract in `docs/observability-contract.md`; gateway exports must cover every migrated database before its cutover | Passing sandbox restore/failure/resource matrices and owner approval |
| Commercial scope | Vendor documentation can expose useful operational patterns | Reject billing, SSO, organizations, and SaaS tenancy from the single-owner product boundary | Explicit product-scope change, not migration pressure |

Public reference entry points are
[docs.turso.tech](https://docs.turso.tech/) and the
[libSQL project](https://github.com/tursodatabase/libsql); every future vendor
claim must carry its access date and an `UNVERIFIED` marker until checked.

## Operator-fillable inventory (empty — fill during survey)

Copy per **source** (Turso) database:

```
- turso database:          (name/URL in Turso)
- gateway target name:     (fresh name, never tombstoned, ≤64 chars)
- file size (bytes):       (must fit the 8 MiB import envelope)
- tables/views/triggers:   (views/triggers MUST be none or redesigned — risk 3)
- indexes/row counts:      (per-table counts for post-import comparison)
- libsql-only features:    (syntax/functions with no gateway equivalent)
- max statement args:      (audit against the 256 cap)
- max result rows:         (audit against the 1000-row cap)
- max SQL bytes:           (audit against the 64 KiB cap)
- app writers:             (which applications write, how often)
- app readers:             (which applications read, latency needs)
- credentials to replace:  (Turso tokens → gateway key names + roles)
- idempotency adoption:    (which callers will send Idempotency-Key)
- recovery requirement:    (RPO/RTO per docs/recovery-decisions.md)
```

## Local fixture rehearsal and its limits

`tests/migration_fixture.rs` imports a synthetic external SQLite file
(authors/books tables, two indexes, a foreign key, INTEGER/REAL/TEXT/BLOB/
NULL/JSON values) through the existing import API, verifies exact restored
rows plus surviving indexes, and proves a fixture smuggling a `_dg_shadow`
table is rejected (`invalid_request`) with nothing published. Read it as a
**trial-import rehearsal**: this is the shape a cleaned Turso export must
take to enter the gateway. Limits: the fixture is synthetic and tiny; it
exercises the gateway import path only. It says nothing about Turso's export
format, dialect, limits, auth, or latency, and must never be cited as
migration evidence.

## Unsupported assumptions (do not assume)

- That a Turso export imports byte-for-byte (export format, page-size
  details, and denied objects need a disposable trial import per database).
- That any current Turso pricing, plan limit, or feature list in memory or
  in older notes is still accurate.
- That the TS client (`client/src/index.ts`) ports callers by changing a
  base URL — the protocols differ completely (risk 1).
- That sources using views, triggers, or `ALTER`-dependent workflows migrate
  without redesign (risk 3).
- That migration can proceed before the deployment-readiness gates
  (`docs/deployment-readiness.md`) are resolved — migration presumes a
  running gateway with backup coverage.
