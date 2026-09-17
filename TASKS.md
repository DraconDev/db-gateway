# DB Gateway — implementation task list

Project: **DB Gateway** · CLI: **`dg`**

## Agreed scope

A personal, single-owner SQLite database service with a Rust gateway and CLI. Applications remain stateless and connect through the API. One gateway manages many independent databases and development copies.

Deployment target: Oracle **micro2**, an approximately empty **1 GB RAM** server. Live SQLite files reside on persistent local disk; persistent database backups go to the owner's **OVH bucket**. Bucket compatibility and backup behaviour must be verified, not assumed.

One owner/admin credential plus database-scoped read-only and read/write application credentials. No multi-organization system, billing, SSO, or web dashboard in the initial release. Personal scope does not imply relaxed durability or correctness.

This file is a plan, not authorization to deploy, migrate production data, delete databases, or change subscriptions. Commands below describe proposed behaviour.

## Execution order and current status

This is the canonical project checklist. Mark items complete only with implementation or documentation evidence and the relevant passing checks. The phases below are a roadmap, not ten tasks to implement in one pass.

### M0 — local foundation (complete, pending review)

- [x] Add a Rust workspace exposing the `dg` executable.
- [x] Provide `dg --help`, `dg --version`, and a loopback-only `dg serve` with a minimal liveness endpoint; no SQL or management routes yet.
- [x] Validate startup configuration and provide structured errors/logging without credentials.
- [x] Add automated tests, formatting/lint checks, CI, and reproducible local instructions.
- [x] Record initial architecture, non-goals, and pending decisions in `docs/architecture.md`.

Acceptance: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked` pass. A local server returns the documented liveness response. This milestone is not a usable database service.

Evidence 2026-09-17: fmt/clippy/tests clean (5 tests), live `curl` against a randomly bound loopback port returned `200 {"status":"ok"}`, SIGTERM exited 0, working tree clean. A test asserts `/v1/databases` is still absent.

### M1 — first authenticated database round trip (complete, pending review)

Per TASKS.md, implemented over the M0 foundation with confirmed decisions: custom JSON HTTP API, TypeScript first client, `Idempotency-Key` support, full M1 scope (owner bootstrap via local `dg init`, revocable database-scoped keys, `dg list/create/query/batch/key`, parameterized statements, atomic batches, bounded execution, restart persistence, negative-path tests, TS client integration suite).

Evidence 2026-09-17: `cargo test --workspace --locked` 16 passed (unit incl. invalid SQL/args, deadline cancellation with rollback+recovery, per-principal idempotency capacity, parallel independent databases; CLI; 6-scenario E2E over real server processes: round trip + restart, scoped-key denials across create/list/issue/revoke, batch rollback + result limits, idempotent replay + conflict, bootstrap refusal, and `cli_commands_against_running_server` exercising `dg create/list/query` with parameterized args, `--json`, and nonzero failure exits); `cargo fmt/clippy -D warnings` clean; root `npm test` integration suite passes. Committed as `8c17d1a`, `6e03ede`, `36349a5`.

Audit repair evidence 2026-09-17: Rust 16/16 and root `npm test` 2/2 pass, including the verbatim `docs/api.md` CLI walkthrough on a local server (valid `bot_a`, exact key response, tables, batch, revocation). Type-check regressions reject integer-object arguments; live SQL tests verify supported values and object rejection for execute/batch. Both app roles are tested across all four management routes. fmt/clippy remain clean.

### Subsequent milestones

1. **M2 — safe lifecycle:** phase 6; clone/export/import/delete concurrency and failure tests.
2. **M3 — verified recovery:** phase 7; backup failure visibility and a verified restore to a separate target.
3. **M4 — release readiness:** phases 8–9; measured 1 GB resource budget, failure tests, packaging, then explicitly approved micro2 deployment.
4. **M5 — migration:** phase 10; separately approved cutovers with verified rollback artifacts.

### Decisions to resolve before dependent work

- **Before M1 protocol implementation:** inventory the first application's client language and any required `@libsql/client` compatibility; choose custom HTTP or protocol compatibility explicitly.
- **Before M1 writes:** document value encoding, transaction boundaries, ambiguous outcomes, and retry/idempotency behavior.
- **Before M3:** agree recovery objectives and verify the actual OVH endpoint/tool version. Do not promise a backup interval as a guaranteed data-loss bound.
- **Before deployment:** confirm micro2 architecture/disk/network details and measure the complete stack's memory consumption.

These decisions do not block the protocol-neutral M0 foundation. No external infrastructure or production credentials are needed for M0.

## 1. Define the initial contract

- [x] Document API routes, SQL value encoding, errors, transaction semantics, and CLI command structure. (`docs/api.md`)
- [x] Inventory existing client requirements before deciding between a custom protocol/client and libSQL compatibility.
- [x] Define write acknowledgement, ambiguous timeout outcomes, retry behaviour, and idempotency support.
- [ ] Define recovery objectives and acceptable downtime/data-loss window; distinguish local durability from asynchronous bucket backup.
- [x] Establish an initial memory/concurrency budget for the complete 1 GB deployment.

Done when: the initial architecture and explicit non-goals are documented, with unresolved compatibility and durability decisions identified.

## 2. Establish the Rust workspace

- [ ] Scaffold the gateway and CLI, preferably exposed through one `dg` executable with subcommands.
- [ ] Add configuration validation, structured errors/logging, formatting, linting, tests, and CI.
- [ ] Separate HTTP handling, database execution, lifecycle management, and credential handling.
- [ ] Add local development instructions with no production credentials required.

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

- [x] Implement server/profile configuration and protected local credential storage.
- [ ] Implement `dg list`, `create`, `query`, and an interactive `shell` with schema/table inspection. (List/create/query/key shipped; interactive `shell` deferred to M2.)
- [x] Add stable JSON output, exit codes, and noninteractive operation for scripts and coding agents.
- [x] Provide a minimal client for the first actual application language, based on the compatibility decision.
- [x] Document that automated development clients should use scoped dev credentials rather than owner access. (`docs/api.md` credential model; `README.md` TS client section)

Done when: CLI and client integration tests create a database, write/read data, and report errors correctly through the same API. (Met by the M1 CLI and TS client integration tests; the interactive `shell` remains open for M2.)

Done when: CLI and client integration tests create a database, write/read data, and report errors correctly through the same API.

## 6. Implement safe database lifecycle operations

- [ ] Implement consistent clone and export using SQLite-aware backup mechanisms, not raw copying of active WAL databases.
- [ ] Implement validated import into a new database with integrity checks and atomic publication.
- [ ] Coordinate clone, delete, import, and restore with active connections and requests.
- [ ] Require explicit destructive-operation intent; define deletion and backup-retention behaviour separately.
- [ ] Add corresponding CLI commands and tests for concurrent lifecycle operations.

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

- [ ] Verify host architecture, persistent disk, available resources, and network approach before selecting release artifacts.
- [ ] Package reproducible installation/configuration for the gateway and backup component.
- [ ] Run under a dedicated non-root service account with restart policy, graceful shutdown, and resource limits.
- [ ] Configure HTTPS or private access, protected secrets, and restricted listening interfaces.
- [ ] Document upgrades, rollback, monitoring, and full host replacement/restore.
- [ ] With explicit deployment approval, run a non-production smoke test and restore drill on micro2.

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
