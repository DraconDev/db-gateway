# Turso → gateway migration inventory + checklist

Status: **preparation only**. No Turso account, database, or application
repository was accessed. The gateway is the migration **target**: Turso
databases move into it, and Turso is decommissioned only after a successful
soak (see `docs/migration-cutover.md`). Notes below are a dated,
public-source reference inventory of the concerns that affect bringing a
Turso database under this gateway; they are not a claim to reproduce Turso's
entire product catalog. Anything marked UNVERIFIED must be confirmed against
current Turso docs or a disposable trial export before any migration plan.

## Source survey 2026-10-11 (corrected): Turso estate found in dracon-platform

Scope: `~/.turso` contents, the `/home/dracon/Dev` tree (bare-word
`turso|libsql|hrana` over code/config/scripts, then table/env/builder
greps), and this repo's references. Method is local-code-only: no Turso
account, cloud API, token value, or row contents were touched — remote
sizes, row counts, and live schemas below are marked UNVERIFIED. An
earlier revision of this section claimed zero sources because its
patterns (`@libsql/client`, `libsql/`) missed Rust `libsql =` deps and
`libsql://` URLs; that claim is withdrawn.

### Source A — email-api database (`TURSO_EMAIL_URL` / `TURSO_EMAIL_TOKEN`)

- gateway target name: (unassigned — Stage 1 has not run)
- file size: UNVERIFIED (remote; no local copy)
- tables: `email_queue` (production DDL,
  `apis/services/email-api/src/state.rs:337`); further remote tables, if
  any: UNVERIFIED
- denied objects: 3× `ALTER TABLE email_queue ADD COLUMN` in production
  migration code (email-api state.rs:361,367,374) — must be flattened to
  gateway-compatible DDL before import (risk 3); production views,
  triggers, virtual tables: none found
- client/writers: `email-api` via `api_core::db::SharedDb`
  (`build_connection`), with a connection supervisor that rebuilds the
  connection (comment in state.rs: server drops idle/rotated Hrana
  streams; libsql 0.9 has no autoreconnect)
- libSQL-specific: Hrana remote protocol (`libsql://` URL + token auth),
  long-lived remote stream with supervisor rebuild
- credentials to replace: `TURSO_EMAIL_URL`/`TURSO_EMAIL_TOKEN` → gateway
  key names + roles (unassigned)
- idempotency adoption: none yet — email-api has no `Idempotency-Key`
equivalent; queue writers must adopt keyed writes (risk 8)

### Source B — studio-api database (`STUDIO_API_DB_URL`)

- gateway target name: (unassigned)
- file size / row counts: UNVERIFIED (remote)
- tables, code-grounded live set of **22** (production DDL only; the
dropped `studio_youtube_oauth_states` is excluded — youtube.rs:176,826).
  The CREATE statements at youtube.rs:304, 337, 352 and analytics.rs:42
  are transient migration tables (`studio_youtube_tokens_v3`,
  `studio_competitors_v2`, `studio_youtube_snapshots_v2`) created and
  immediately renamed to the live name in the same `execute_batch` — not
  counted as live schemas (youtube.rs:330-331, 345-346, 364-365;
  analytics.rs:53-54). Live set:
  - state.rs:805–1404 (19 `CREATE TABLE IF NOT EXISTS`):
    `studio_competitors`, `studio_competitor_snapshots`,
    `studio_channel_presets`, `studio_research_saved`,
    `studio_video_snapshots`, `studio_daily_titles`,
    `studio_video_latest`, `studio_video_prev`,
    `studio_channel_language`, `studio_channel_growth`,
    `studio_channel_profile`, `studio_video_profile`, `studio_follows`,
    `studio_sweep_state`, `studio_watch_groups`,
    `studio_watch_group_items`, `studio_saved_filters`,
    `studio_saved_comparisons`, `studio_last_seen`
  - youtube.rs: `studio_youtube_tokens`, `studio_youtube_preferences`
  - analytics.rs: `studio_youtube_snapshots` (v1)
  Migration artifacts not counted: `studio_youtube_tokens_v3`
  (youtube.rs:304,337), `studio_competitors_v2` (youtube.rs:352),
  `studio_youtube_snapshots_v2` (analytics.rs:42).
- denied objects (corrected — an earlier revision wrongly claimed
  production triggers): production `CREATE TRIGGER` = none; both
  studio-api trigger hits are test fixtures
  (`src/handlers/account_delete/tests.rs`, `src/memory_atomic_tests.rs`).
  `CREATE VIEW` / virtual tables = none found. `ALTER TABLE` IS
  production code (10× state.rs, 3× youtube.rs, 1× analytics.rs) and must
  be flattened to gateway-compatible DDL before import (risk 3 fires via
  ALTER, not triggers); remote-side views: UNVERIFIED
- client/writers: `studio-api` handlers plus a dedicated poller
  connection, via `libsql::Builder::new_remote` (state.rs:642, poller
  state.rs:680; URL from `STUDIO_API_DB_URL`, state.rs:619); dev-local
  fallback history (`:memory:?cache=shared` references)
- libSQL-specific: Hrana remote protocol; no embedded-replica use found
  in code
- credentials to replace: `STUDIO_API_DB_URL` + token → gateway keys
  (unassigned)
- idempotency adoption: none yet (risk 8)

### Source C — billing databases (`dracon-billing`, `dracon-billing-dev`)

- URLs (code-grounded, `apis/tools/migrate-turso-omega-cleanup.py`):
  `libsql://dracon-billing-dev-dracondev.aws-eu-west-1.turso.io`,
  `libsql://dracon-billing-dracondev.aws-eu-west-1.turso.io`
- tables, code-grounded (billing-api state.rs `CREATE TABLE` set):
  `ai_policies`, `ai_pool_accounting`, `ai_reservations`,
  `ai_usage_accounting`, `current_usage`,
  `entitlement_event_watermarks`, `failed_email_notifications`,
  `processed_webhook_events`, `service_access`, `software_licenses`,
  `tier_limits`, `users` (12 tables; full column lists + row counts:
  UNVERIFIED)
- denied objects: production `ALTER TABLE ... DROP COLUMN` migration
  code (billing-api state.rs:847,866,932); production views/triggers:
  none found; remote-side views: UNVERIFIED
- client/writers: `billing-api` (`apis/services/billing-api/src/state.rs`
  references the billing DB, state.rs:560; credentials `TURSO_BILLING_URL` /
  `TURSO_BILLING_TOKEN` per apis/docs/archive/2026-06-15 deploy + audit
  docs); the migrate script is one-shot, not an
  ongoing writer
- libSQL-specific: Hrana remote protocol; rest UNVERIFIED
- credentials, sizes, idempotency: UNVERIFIED / unassigned

### Explicitly out of scope — 14 confirmed-dead databases

`momo-ai-memory`, `momo-ai-models`, `momo-ai-models-staging`,
`momo-kami`, `momo-kami-staging`, `momo-pages`, `momo-pages-staging`,
`momo-pages-v2`, `momo-pages-v2-staging`, `my-db`, `next-js-prac`,
`next-saas-starter`, `simple-stripe-sub-and-advanced-checkout`, `test`
— verified dead with empty schemas by
`dracon-platform/scripts/turso-delete-dead-dbs.sh` (zero code refs, zero
traffic, slated for deletion). No import, no gateway target. Kept but
shelved: `ai-books`, `ai-model-list` (borderline product ideas — survey
when activated).

### Survey limits

Remote row counts, file sizes, and live remote schemas were not read
(no cloud access from this survey). `~/.turso` on this host holds only
downloaded `sqld`/`turso` binaries. The gateway's own test/state SQLite
files are rehearsal artifacts, not sources. Cutover gate unchanged:
every source above needs a filled block (sizes, denied-object census,
writers, key roles, retry behavior, recovery requirements) before its
cutover — currently code-grounded but remote-UNVERIFIED.

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
