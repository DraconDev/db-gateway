# Readiness summary — local preparation and supervised micro2 evidence

> **Dated snapshot: 2026-09-20 — superseded, kept as evidence.** The gate
> counts and "explicitly unperformed live work" below describe that date
> only. Since then (through 2026-10-19): OVH compatibility verified live
> 12/12 (`sandbox-packet-2026-10-14-ovh.log`); the Litestream sidecar is
> live on micro2 (`litestream-replication-evidence.log`) with automatic
> new-database coverage (`litestream-sync-drill-2026-10-19.log`); micro2
> steady-state resources were sampled by script
> (`m4-micro2-sampling-2026-10-16.log`); a pilot database has soaked
> since 2026-10-16 (`pilot-soak-log.md`); the backup watchdog digest is
> live (`watchdog-2026-10-17.log`); the test baseline is 212 Rust tests
> (plus 16 client, 39 web). Current status lives in
> [`TASKS.md`](../TASKS.md), [`deployment-readiness.md`](deployment-readiness.md),
> and the findings register in [`audit-2026-10-19.md`](audit-2026-10-19.md).
> Production readiness is **still not accepted**.

Updated: 2026-09-20. This is an evidence-backed summary of completed local
preparation plus the explicitly authorized supervised micro2 smoke/restore
deploy. It is **not a claim of production readiness**. The authoritative
host/service facts are in [`first-deploy-micro2.md`](first-deploy-micro2.md);
this summary does not authorize any further live change.

## Gates re-run for this summary

| Gate | Command | Result |
|---|---|---|
| Rust tests | `timeout 280 cargo test --workspace --locked` | 82 passed, 0 failed (38 suites) |
| Clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | clean |
| Formatting | `cargo fmt --all -- --check` + `git diff --check` | clean |
| TS client | `npm test` (in `client/`) | 5 passed, 0 failed |

## Delivered preparation artifacts

Resilience (tests): `tests/recovery_drill.rs` (snapshot drill),
`tests/recovery_rehearsal.rs` + `src/bin/recovery_rehearsal.rs` (local
Profile C fault/status/fallback/restore rehearsal),
`tests/crash_restart.rs` (kill/restart ack + keyed replay),
`client/test/lost_ack.test.mjs` (no silent resends), deadline/contention/
boundary/publish-failure regressions in `tests/m1_e2e.rs`,
`tests/m2_lifecycle.rs`, `src/execution.rs`, `src/store.rs`,
`tests/logging_canary.rs` (no credential/SQL leakage),
`tests/migration_fixture.rs` (synthetic import + rejection).

Measurement & packaging: `scripts/resource-report.sh` (+ smoke test,
`docs/resource-harness.md`), `docs/resource-budget.md` (conditional
not-ready), `scripts/release-bundle.sh` (+ smoke test),
`deploy/dg.service` (+ `scripts/validate-deploy.sh`, `docs/deployment.md`),
`docs/upgrade-rollback.md` (rehearsed locally with recorded results).

Deployment/migration prep: `docs/deployment-readiness.md` (8 gates,
reconciled to the supervised micro2 record; production readiness remains not
accepted), `docs/migration-inventory.md` (11 risks + empty inventory),
`docs/migration-cutover.md` (staged plan + rollback),
`docs/recovery-decisions.md` (working Profile C objectives, production
acceptance still pending), `docs/recovery-contract.md`,
`docs/observability-contract.md`, `docs/recovery-rehearsal-evidence.json`,
`docs/backup-compatibility.md` (Litestream v0.5.17 pinned, endpoint
unverified). The supervised micro2 install, restart check, smoke query, and
throwaway export/import drill are recorded in `docs/first-deploy-micro2.md`.

## Reconciliation

Done locally: recovery contract + drills, crash/restart behavior, replay and
lost-ack discipline, deadline/contention/boundary/publish-failure coverage,
interrupted-publish invariant, logging-canary audit, resource harness +
budget ledger, release bundle, service template + validator, upgrade/rollback
rehearsal, deployment checklist, migration inventory/fixture/cutover plan.

Remaining owner decisions: production acceptance of the Profile C
RPO/RTO/retention objectives, complete resource budget and memory-limit value,
backup-sidecar plan, rollback/soak choices, future TLS/monitoring needs, and
Turso plan/target/trial verification. The
micro2 architecture/disk survey and SSH-tunnel access choice are evidenced for
the supervised deploy; they do not authorize production migration or future
live changes.

## Explicitly unperformed live work (as of 2026-09-20; see the superseding note above)

- OVH: no bucket survey, no Litestream deployment, no backup/restore to or
  from OVH, no credential or encryption setup.
- micro2: the first supervised deploy is complete and recorded: host survey,
  installed active service, restart survival, authenticated smoke query, and
  throwaway export/import/copy-independence drill. No backup sidecar, TLS
  terminator, monitoring, production migration, or further live change was
  authorized or performed.
- Turso: no account/database access, no trial import, no application
  repository survey, no cutover of any kind.
- No production data was read, written, or moved at any point.
