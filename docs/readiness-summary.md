# Readiness summary — local preparation and supervised deployment-host evidence

> **Dated snapshot: 2026-09-20 — superseded, kept as evidence.** The gate
> counts and "explicitly unperformed live work" below describe that date
> only. Since then (through 2026-10-19): off-host storage compatibility verified live
> 12/12 (sandbox packet record); off-host replication is
> live on deployment-host (replication record) with automatic
> new-database coverage (sync-drill record); deployment-host
> steady-state resources were sampled by script
> (sampling record); an example database has undergone verification
> since 2026-10-16 (pilot log); the backup watchdog digest is
> live (watchdog record); the test baseline is 212 Rust tests
> (plus 16 client, 39 web). Current status lives in
> [`TASKS.md`](../TASKS.md), [`deployment-readiness.md`](deployment-readiness.md),
> and the findings register in [`audit-2026-10-19.md`](audit-2026-10-19.md).
> Production readiness is **still not accepted**.

Updated: 2026-09-20. This is an evidence-backed summary of completed local
preparation plus the explicitly authorized supervised deployment-host smoke/restore
deploy. It is **not a claim of production readiness**. The authoritative
host/service facts are in the local deployment record;
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
`tests/m2_lifecycle.rs`, `src/execution.rs`, `src/store/`,
`tests/logging_canary.rs` (no credential/SQL leakage),
`tests/migration_fixture.rs` (synthetic import + rejection).

Measurement & packaging: `scripts/resource-report.sh` (+ smoke test,
`docs/resource-harness.md`), `docs/resource-budget.md` (conditional
not-ready), `scripts/release-bundle.sh` (+ smoke test),
`deploy/dg.service` (+ `scripts/validate-deploy.sh`, `docs/deployment.md`),
`docs/upgrade-rollback.md` (rehearsed locally with recorded results).

Deployment/migration prep: `docs/deployment-readiness.md` (8 gates,
reconciled to the supervised deployment-host record; production readiness remains not
accepted), `docs/migration-inventory.md` (11 risks + empty inventory),
`docs/migration-cutover.md` (staged plan + rollback),
`docs/recovery-decisions.md` (working Profile C objectives, production
acceptance still pending), `docs/recovery-contract.md`,
`docs/observability-contract.md`, `docs/backup-compatibility.md` (replication version pinned, endpoint
unverified). The supervised deployment-host install, restart check, smoke query, and
throwaway export/import drill are recorded in the local deployment record.

## Reconciliation

Done locally: recovery contract + drills, crash/restart behavior, replay and
lost-ack discipline, deadline/contention/boundary/publish-failure coverage,
interrupted-publish invariant, logging-canary audit, resource harness +
budget ledger, release bundle, service template + validator, upgrade/rollback
rehearsal, deployment checklist, migration inventory/fixture/cutover plan.

Remaining operator decisions: production acceptance of the Profile C
recovery objectives/retention objectives, complete resource budget and memory-limit value,
backup plan, rollback/verification choices, future TLS/monitoring needs, and
Turso plan/target/trial verification. The
deployment-host architecture/disk survey and private transport-tunnel access choice are evidenced for
the supervised deploy; they do not authorize production migration or future
live changes.

## Explicitly unperformed live work (as of 2026-09-20; see the superseding note above)

- off-host storage: no bucket survey, no replication deployment, no backup/restore to or
  from off-host storage, no credential or encryption setup.
- deployment-host: the first supervised deploy is complete and recorded: host survey,
  installed active service, restart survival, authenticated smoke query, and
  throwaway export/import/copy-independence drill. No backup sidecar, TLS
  terminator, monitoring, production migration, or further live change was
  authorized or performed.
- Turso: no account/database access, no trial import, no application
  repository survey, no cutover of any kind.
- No production data was read, written, or moved at any point.
