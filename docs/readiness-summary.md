# Readiness summary — local preparation queue close-out

Date: 2026-09-18. This is an evidence-backed summary of completed local
preparation, **not a claim of production readiness**. No OVH, micro2, or
Turso system was accessed; all evidence below is local or explicitly marked.

## Gates re-run for this summary

| Gate | Command | Result |
|---|---|---|
| Rust tests | `timeout 280 cargo test --workspace --locked` | 52 passed, 0 failed (13 suites) |
| Clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | clean |
| Formatting | `cargo fmt --all -- --check` + `git diff --check` | clean |
| TS client | `npm test` (in `client/`) | 5 passed, 0 failed |

## Delivered preparation artifacts

Resilience (tests): `tests/recovery_drill.rs` (snapshot drill),
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

Deployment/migration prep (docs only): `docs/deployment-readiness.md`
(8 gates, not-accepted verdict), `docs/migration-inventory.md` (11 risks +
empty inventory), `docs/migration-cutover.md` (staged plan + rollback),
`docs/recovery-decisions.md`, `docs/recovery-contract.md`,
`docs/backup-compatibility.md` (Litestream v0.5.17 pinned, endpoint
unverified).

## Reconciliation

Done locally: recovery contract + drills, crash/restart behavior, replay and
lost-ack discipline, deadline/contention/boundary/publish-failure coverage,
interrupted-publish invariant, logging-canary audit, resource harness +
budget ledger, release bundle, service template + validator, upgrade/rollback
rehearsal, deployment checklist, migration inventory/fixture/cutover plan.

Remaining owner decisions: RPO/RTO/retention acceptance, micro2 survey
answers (arch/disk/network), remote-access mechanism (tunnel vs proxy, no
TLS in-gateway), memory-limit value, rollback/soak choices, Turso
plan/target/trial verification.

## Explicitly unperformed live work

- OVH: no bucket survey, no Litestream deployment, no backup/restore to or
  from OVH, no credential or encryption setup.
- micro2: no host survey, no installation, no service start, no smoke test
  or restore drill on the host.
- Turso: no account/database access, no trial import, no application
  repository survey, no cutover of any kind.
- No production data was read, written, or moved at any point.
