# DB Gateway — implementation task list

Project: **DB Gateway** · CLI: **`dg`**

## Agreed scope

A personal, single-owner SQLite database service with a Rust gateway and CLI. Applications remain stateless and connect through the API. One gateway manages many independent databases and development copies.

Deployment target: Oracle **micro2**, an approximately empty **1 GB RAM** server. Live SQLite files reside on persistent local disk; persistent database backups go to the owner's **OVH bucket**. Bucket compatibility and backup behaviour must be verified, not assumed.

One owner/admin credential plus database-scoped read-only and read/write application credentials. No multi-organization system, billing, SSO, or web dashboard in the initial release. Personal scope does not imply relaxed durability or correctness.

This file is a plan, not authorization to deploy, migrate production data, delete databases, or change subscriptions. Commands below describe proposed behaviour.

## Execution order and current status

This is the canonical project checklist. Mark items complete only with implementation or documentation evidence and the relevant passing checks. The phases below are a roadmap, not ten tasks to implement in one pass.

### M0 — local foundation (implemented; historical evidence)

The original foundation is present in [Cargo.toml](Cargo.toml), [src/lib.rs](src/lib.rs), [tests/cli.rs](tests/cli.rs), and [CI](.github/workflows/ci.yml). No standalone M0 approval archive is available locally; subsequent approved milestones include this foundation. The route-only statements and five-test count below describe M0 at that time, not today's authenticated server.

- [x] Add a Rust workspace exposing the `dg` executable.
- [x] Provide `dg --help`, `dg --version`, and a loopback-only `dg serve` with a minimal liveness endpoint; no SQL or management routes yet.
- [x] Validate startup configuration and provide structured errors/logging without credentials.
- [x] Add automated tests, formatting/lint checks, CI, and reproducible local instructions.
- [x] Record initial architecture, non-goals, and pending decisions in `docs/architecture.md`.

Acceptance: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked` pass. A local server returns the documented liveness response. This milestone is not a usable database service.

Evidence 2026-09-17: fmt/clippy/tests clean (5 tests), live `curl` against a randomly bound loopback port returned `200 {"status":"ok"}`, SIGTERM exited 0, working tree clean. A test asserts `/v1/databases` is still absent.

### M1 — first authenticated database round trip (approved)

Approval: [M1 archive](.pi-glla/archive/20260917185718-ll0713.md). Behavioral evidence: [M1 E2Es](tests/m1_e2e.rs), [execution tests](src/execution.rs), and [TypeScript integration tests](client/test/integration.test.mjs). Counts below are historical milestone evidence.

Per TASKS.md, implemented over the M0 foundation with confirmed decisions: custom JSON HTTP API, TypeScript first client, `Idempotency-Key` support, full M1 scope (owner bootstrap via local `dg init`, revocable database-scoped keys, `dg list/create/query/batch/key`, parameterized statements, atomic batches, bounded execution, restart persistence, negative-path tests, TS client integration suite).

Evidence 2026-09-17: `cargo test --workspace --locked` 16 passed (unit incl. invalid SQL/args, deadline cancellation with rollback+recovery, per-principal idempotency capacity, parallel independent databases; CLI; 6-scenario E2E over real server processes: round trip + restart, scoped-key denials across create/list/issue/revoke, batch rollback + result limits, idempotent replay + conflict, bootstrap refusal, and `cli_commands_against_running_server` exercising `dg create/list/query` with parameterized args, `--json`, and nonzero failure exits); `cargo fmt/clippy -D warnings` clean; root `npm test` integration suite passes. Committed as `8c17d1a`, `6e03ede`, `36349a5`.

Audit repair evidence 2026-09-17: Rust 16/16 and root `npm test` 2/2 pass, including the verbatim `docs/api.md` CLI walkthrough on a local server (valid `bot_a`, exact key response, tables, batch, revocation). Type-check regressions reject integer-object arguments; live SQL tests verify supported values and object rejection for execute/batch. Both app roles are tested across all four management routes. fmt/clippy remain clean.

### M2 — safe lifecycle operations (approved)

Approval: [M2 archive](.pi-glla/archive/20260917203823-isball.md). Behavioral evidence: [lifecycle E2Es](tests/m2_lifecycle.rs), [M1/lifecycle E2Es](tests/m1_e2e.rs), and [store tests](src/store.rs). The repair history below records earlier test totals; it is not pending work.

Implemented per phase 6 with confirmed decisions: tombstone + delayed purge deletion (owner-only, 24h retention, exact typed-name `--confirm NAME` gate) and `.sqlite` snapshot exports via SQLite's online backup API. Routes: `POST /v1/databases/{name}/clone`, `POST /v1/databases/{name}/export` (base64, 8 MiB cap), `POST /v1/databases/{name}/import` (validated + atomic publication, 12 MiB cap), `DELETE /v1/databases/{name}`. CLI: `dg clone/export/import/delete` (delete requires `--confirm NAME`). Import rejects corrupted files, foreign-key violations, and smuggled `_dg_*` tables (own `_dg_replay` excepted).

Evidence 2026-09-17: `cargo test --workspace --locked` 22 passed — unit incl. store-level clone round trip and import validation; E2E incl. `clone_during_writes_takes_consistent_snapshot` (concurrent writer + clone), `failed_import_and_missing_source_never_publish` (garbage payload, missing source, tombstoned target), `lifecycle_clone_export_import_delete` (clone/export/import/delete/tombstone 404 + recreate 409 + double-delete 409 + app-key delete 403), `cli_lifecycle_commands_against_running_server` (real CLI: clone, export to file, import, delete confirmation gate, tombstoned query 404). fmt/clippy clean. Committed across the M2 commit range from `fe2c72c`.

Audit repair evidence 2026-09-17: `cargo test --workspace --locked` **28 passed, 0 failed**; fmt check and Clippy `-D warnings` pass; root `npm test` **2 passed, 0 failed**. Tombstones disappear from listing; both scoped roles get 404 while retaining access to another database. CLI rejects absent/mismatched name confirmation and `--yes`. Actual open WAL transactions prove overlap with clone/delete; clone asserts exact committed rows and independent integrity. Request-owned staging and simultaneous same-source exports / same-target clone/import tests verify isolation. An idle live server purges an expired test tombstone without lifecycle traffic, retains an unexpired one, and permits safe recreation. A request-lease test prevents purge until the active connection closes. Import-specific body-limit coverage includes a snapshot above 64 KiB. See `tests/m2_lifecycle.rs`, `tests/m1_e2e.rs`, and `src/store.rs` tests.

Second audit repair 2026-09-17: imported external SQLite files are prepared with the replay schema before publication, so M1 keyed writes and replays work on imported databases; any pre-existing `_dg_replay` table must match the canonical definition exactly (normalized comparison) or the import is rejected before publication, and foreign-key violations reject imports with negative tests for both (`imported_replay_table_must_match_canonical_schema`, `foreign_key_violations_reject_import` in src/execution.rs). The regression `imported_external_sqlite_file_supports_m1_keyed_writes` (tests/m2_lifecycle.rs) now sends a real `Idempotency-Key` header and uses a non-idempotent INSERT: the replay returns the stored original response and the row exists exactly once after both requests. `cargo test --workspace --locked` **31 passed, 0 failed**; fmt/clippy clean; `npm test` 2/2.

### Interactive shell (approved)

Approval: [shell archive](.pi-glla/archive/20260917221933-q22wef.md). Implementation: [src/shell.rs](src/shell.rs); contract: [shell reference](docs/api.md#interactive-shell). Four [shell E2Es](tests/shell_e2e.rs) cover database selection, query/error/recovery, batches, JSON, schema inspection, keyed replay, and lost acknowledgements without retries. Three unit tests cover prompts, oversized input, and output failure. The approved shell snapshot records **38 Rust tests passed**, fmt/Clippy clean, and **2 TypeScript integration tests passed**. This is archived evidence, not a new test run for this checklist edit.

### Local preparation queue — evidence (2026-09-18, not live acceptance)

Summary: [readiness summary](docs/readiness-summary.md). Gates re-run then: Rust 52/52, Clippy clean, fmt/diff clean, TS client 5/5. Delivered locally: recovery contract + snapshot/crash/keyed-replay/lost-ack/deadline/contention/boundary/publish-failure/interrupted-publish/logging-canary regressions; resource harness + budget ledger (conditional not-ready); release bundle + smoke test; systemd template + validator; upgrade/rollback runbook with recorded local rehearsal; deployment-readiness checklist (reconciled, production not accepted); migration inventory + synthetic fixture + cutover plan. A separately authorized supervised micro2 deploy, restart check, smoke query, and throwaway restore drill are recorded in [the first-deploy record](docs/first-deploy-micro2.md). Live phases 7 (OVH), 9 (full micro2 release readiness), and 10 (Turso) remain incomplete below; no production data moved and no future live action is authorized by this plan. Remaining owner decisions are listed in the summary.

### Remaining milestones and authorization boundaries

1. **M3 — verified recovery:** phase 7; owner recovery decisions, backup failure visibility, and verified restore to a separate target. Local snapshot tests do not establish OVH coverage.
2. **M4 — release readiness:** phases 8–9; measured 1 GB resource budget, failure tests, packaging, and any future micro2 live action separately approved. The first supervised non-production smoke/restore deploy is recorded, but does not close full release readiness.
3. **M5 — migration:** phase 10; separately approved cutovers with verified rollback artifacts.

The confirmed preparation queue permits local disposable fixtures and public research only. It includes recovery decision/compatibility briefs, local drills and resilience tests, resource measurements, packaging, and deployment/migration preparation. The first supervised micro2 access/deploy was separately approved and is recorded; this plan does **not** authorize any additional OVH/micro2/Turso access, remote writes, upgrades, deployments, production migration, or subscription changes. Live milestone acceptance remains open. Later capabilities below remain optional, not implicitly added release requirements.

### Decisions to resolve before dependent work

- **Resolved in M1:** TypeScript first client and custom JSON HTTP API; no libSQL compatibility promise. See [decision register](docs/architecture.md#required-decisions-and-evidence).
- **Resolved in M1:** value encoding, transaction boundaries, ambiguous outcomes, and retry/idempotency behavior are documented in [API reference](docs/api.md).
- **Before M3:** agree recovery objectives and verify the actual OVH endpoint/tool version. Do not promise a backup interval as a guaranteed data-loss bound.
- **Before production readiness or any further live change:** use the recorded micro2 architecture/disk/network facts, resolve the SSH-tunnel/private-access boundary, and measure the complete stack's memory consumption; the first supervised deploy does not waive these remaining gates.

The remaining decisions do not block local preparation. Accepted production RPO/RTO/retention, verified OVH compatibility, and target-host facts remain prerequisites for dependent live work.

## 1. Define the initial contract

- [x] Document API routes, SQL value encoding, errors, transaction semantics, and CLI command structure. (`docs/api.md`)
- [x] Inventory existing client requirements before deciding between a custom protocol/client and libSQL compatibility.
- [x] Define write acknowledgement, ambiguous timeout outcomes, retry behaviour, and idempotency support.
- [ ] Define recovery objectives and acceptable downtime/data-loss window; distinguish local durability from asynchronous bucket backup.
- [ ] Establish a memory budget for the complete 1 GB deployment. (Initial gateway concurrency/cache/result limits exist in [execution core](src/execution.rs) and [decision register](docs/architecture.md); measured gateway + backup + OS headroom remains unverified.)

Done when: the initial architecture and explicit non-goals are documented, with unresolved compatibility and durability decisions identified.

## 2. Establish the Rust workspace

- [x] Scaffold the gateway and CLI through one `dg` executable with subcommands. ([Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs))
- [x] Add configuration validation, structured errors/logging, formatting, linting, tests, and CI. ([CLI tests](tests/cli.rs), [error types](src/error.rs), [CI](.github/workflows/ci.yml); broader logging-canary verification remains open in phase 4.)
- [x] Separate HTTP handling, database execution, lifecycle management, and credential handling. ([api](src/api.rs), [execution](src/execution.rs), [store](src/store.rs), [client](src/client.rs); credential registry/authorization is owned by Store, not a separate placeholder module.)
- [x] Add local development instructions with no production credentials required. ([README quick start](README.md#quick-start-local-only))

Done when: a clean checkout builds and passes formatting, linting, and starter tests.

## 3. Build the bounded SQLite execution core

- [x] Implement a managed database registry with validated names and explicit creation; queries never silently create databases.
- [x] Configure WAL, foreign-key enforcement, busy handling, and deliberate durability settings on connections.
- [x] Keep blocking SQLite work off async network threads.
- [x] Support parameterized statements and atomic transaction batches with correct rollback and connection cleanup.
- [x] Bound global work, per-database queues, connection/cache counts, request sizes, and result sizes.
- [x] Implement actual query interruption and deadline handling, not only HTTP timeouts.

Done when: query, rollback, contention, cancellation, and bounded-result tests pass, including parallel activity against independent databases. (Met 2026-09-17: committed tests cover invalid SQL/args and protected tables, 2s-deadline interruption with verified rollback + post-interruption recovery, per-principal idempotency capacity with two-principal regression, parallel writes to four independent databases, batch rollback, and 413 result limits.)

## 4. Implement access control and the API

- [x] Bootstrap a single owner securely; store high-entropy credential hashes rather than plaintext keys.
- [x] Support issuing and revoking database-scoped read-only and read/write credentials.
- [x] Enforce database isolation and read-only permissions using SQLite authorization/connection controls, not SQL prefix checks.
- [x] Restrict SQL features that could escape the database scope or change server-owned settings, including attachment, extension loading, and filesystem operations.
- [x] Expose query, transaction, management, and health APIs with consistent error responses.
- [ ] Keep secrets and sensitive SQL values out of logs; document HTTPS/private-network deployment requirements.

Done when: positive and negative authorization tests pass, including cross-database denial, revoked keys, and management operations denied to app credentials.

## 5. Deliver the CLI and minimal client

- [x] Implement explicit server configuration and protected local credential-file access. ([CLI flags](src/main.rs), [read_key](src/client.rs); no named-profile manager is claimed.)
- [x] Implement `dg list`, `create`, `query`, and an interactive `shell` with schema/table inspection. (`src/shell.rs`, `tests/shell_e2e.rs`: persistent database selection, query/batch, tables/schema, text/JSON, recovery and explicit replay.)
- [x] Add stable JSON output, exit codes, and noninteractive operation for scripts and coding agents.
- [x] Provide a minimal client for the first actual application language, based on the compatibility decision.
- [x] Document that automated development clients should use scoped dev credentials rather than owner access. (`docs/api.md` credential model; `README.md` TS client section)

Done when: CLI and client integration tests create a database, write/read data, and report errors correctly through the same API. (Met by the M1 CLI and TS client integration tests; the shell now has four piped-stdin E2Es and three unit tests.)

## 6. Implement safe database lifecycle operations

- [x] Implement consistent clone and export using SQLite-aware backup mechanisms, not raw copying of active WAL databases. (`Store::export_to` uses the online backup API; `clone_during_writes_takes_consistent_snapshot` proves consistency during concurrent writes)
- [x] Implement validated import into a new database with integrity checks and atomic publication. (`execution::validate_import`: integrity_check + foreign-key check + protected-table screening; `Store::import_from` publishes in one registry transaction)
- [x] Coordinate clone, delete, and import with active connections and requests; restore orchestration remains in phase 7. (deletion tombstones make the name unqueryable/uncreatable/unimportable atomically; failed imports/clone temp files are always removed; backup API avoids writer exclusion)
- [x] Require explicit destructive-operation intent; define deletion and backup-retention behaviour separately. (`dg delete NAME --confirm NAME` gate; tombstone + 24h purge_after retention, independent server maintenance documented in docs/api.md)
- [x] Add corresponding CLI commands and tests for concurrent lifecycle operations. (`dg clone/export/import/delete`, `cli_lifecycle_commands_against_running_server`, `lifecycle_clone_export_import_delete`, `failed_import_and_missing_source_never_publish`)

Done when: clones/exports remain consistent during writes, failed operations do not publish partial databases, and deletion cannot race active work into accidental recreation.

## 7. Integrate OVH-backed recovery

- [ ] Verify the OVH bucket's S3 compatibility, region/endpoint, credentials, and pricing relevant to backups.
- [ ] Select and pin a backup tool/version, evaluating Litestream before considering custom backup code.
- [ ] Automate per-database backup registration with distinct destinations; verify supported configuration/reload behaviour.
- [ ] Define retention, credential permissions, encryption requirements, and behaviour when the bucket is unavailable.
- [ ] Expose backup health, last successful backup/recoverable point where available, and lag/staleness through API and CLI.
- [ ] Restore into a separate target by default, with integrity and application-data verification.

Done when: an end-to-end API write → OVH backup → restore to a separate database → verified read succeeds, and backup failures are visible.

## 8. Test failure handling and resource limits

- [ ] Measure gateway, database, backup, and OS resource usage on a representative 1 GB budget.
- [ ] Test slow/expensive queries, oversized results, bursts, and many active/idle databases.
- [ ] Test crash/restart, low disk space, interrupted lifecycle operations, and bucket outage/recovery.
- [ ] Verify a noisy development database does not monopolize the gateway; document limits of shared-host isolation.
- [ ] Test ambiguous write responses and supported retry/idempotency behaviour.
- [ ] Record benchmark workload, durability settings, latency, and memory rather than claiming universal throughput ceilings.

Done when: bounded test results and known limitations are recorded, including recovery evidence and measured memory headroom.

## 9. Package and deploy to micro2

- [x] Verify host architecture, persistent disk, available resources, and network approach before selecting release artifacts. (Supervised micro2 evidence is recorded in `docs/first-deploy-micro2.md`; long-term resource growth remains open.)
- [ ] Package reproducible installation/configuration for the gateway and backup component.
- [ ] Run under a dedicated non-root service account with restart policy, graceful shutdown, and resource limits.
- [ ] Configure HTTPS or private access, protected secrets, and restricted listening interfaces.
- [ ] Document upgrades, rollback, monitoring, and full host replacement/restore.
- [x] With explicit deployment approval, run a non-production smoke test and restore drill on micro2. (`docs/first-deploy-micro2.md` and `docs/first-deploy-micro2-evidence.log`; this does not authorize future live changes.)

Done when: the installed service survives restart and can be recovered using the documented procedure without relying on the original host's files.

## 10. Migrate from Turso safely

- [ ] Inventory databases, schemas, clients, ongoing writers, and any libSQL-specific features.
- [ ] Export and validate databases while existing access remains available.
- [ ] Pilot one development database and then one low-risk application.
- [ ] Plan each production cutover with a write freeze/final export or another verified consistency mechanism; avoid accidental dual writers.
- [ ] Verify schemas, relevant row counts/data, application behaviour, and backup coverage after cutover.
- [ ] Retain rollback artifacts; confirm no clients depend on Turso before any separately authorized cancellation/downgrade.

Done when: migrated applications use DB Gateway successfully, restored copies are verified, and remaining external dependencies are explicitly accounted for.

## Later capabilities — not initial-release blockers

- [ ] Ephemeral databases with TTL, count/memory limits, and explicit crash-loss semantics.
- [ ] Idempotent match-result finalization so retries cannot duplicate rewards; no assumed cross-database atomicity.
- [ ] Optional replay archival and checkpointing where loss of an unfinished match is unacceptable.
- [ ] Placement/movement of independent databases between hosts, only when required by measured workload.
- [ ] Optional visual administration if the CLI proves insufficient.

PostgreSQL remains an application-specific option, not a promise of transparent SQL-engine interchangeability. Automatic failover and synchronous remote durability are not part of the initial single-host design.
