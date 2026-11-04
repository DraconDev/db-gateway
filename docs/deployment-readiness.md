# deployment-host deployment acceptance checklist

Reconciled to the supervised deployment-host deployment record and its verbatim host
evidence log (kept locally, not in the public tree). The first non-production smoke/restore deployment was explicitly
owner-approved and is evidence of what was actually installed and exercised;
it is **not** a blanket production-readiness approval. This checklist does not
authorize any future host change, upgrade, migration, backup/TLS/monitoring
installation, or production-data action.

_Last reconciled 2026-10-19 against the Oct 13–19 measurements (sandbox packet, replication, sampling, pilot verification, backup
watchdog, replication-coverage sync — see gates 5 and 7). Findings and
remaining work are tracked in `docs/audit-2026-10-19.md`._

## Gates

| # | Gate | Status | Evidence / unknown |
|---|---|---|---|
| 1 | Architecture fit | **Verified for supervised deploy, and proven on a clean host** | The recorded target is Ubuntu, x86_64, 2 vCPU, glibc 2.39, with systemd; the deployment-host release bundle's ELF machine and `/lib64/ld-linux-x86-64.so.2` interpreter were verified on-host. See the deployment record (local evidence). The whole first-deploy path was then re-proven from nothing on a disposable clean host built from the same pinned image (clean-host rehearsal record). |
| 2 | Persistent disk | **Verified for current layout; growth unmeasured** | `/var/lib/dg` is `dg:dg`, mode `0700`, with 25 GiB free during the recorded drill; restart persistence was verified. Long-term growth, reboot persistence, and backup-sidecar disk use remain unmeasured. |
| 3 | Network / TLS or private access | **Accepted: private transport** | `dg serve` speaks plain HTTP on local only and refuses non-local binds. Remote CLI use goes through a user-selected private transport (for example `ssh -L 7432:127.0.0.1:7432 <host>`), then `dg --server http://127.0.0.1:7432 --key-file <0600 key>`. For application hosts the edge is now decided (`docs/access-edge.md`): WireGuard mesh with local socket proxies at both ends, gateway bind unchanged; templates exist, nothing installed — install is a separate owner-approved change. Public TLS termination is rejected for this estate. There is no TLS in the gateway. |
| 4 | Service isolation | **Ready and verified on deployment-host** | `deploy/dg.service` passed `scripts/validate-deploy.sh`; the recorded service uses a dedicated `dg` user, `ProtectSystem=strict`, `NoNewPrivileges`, a writable data dir only, and no secrets in the unit. It is active and enabled on the recorded host. |
| 5 | Measured resource budget | **Composed; trend sampling live, multi-day series accumulating** | Scripted 3-pass deployment-host sampling (local sampling record): gateway 6,696 KiB steady, sidecar ~21,300 KiB, restore 446 ms integrity-ok. Composed 2026-10-19 (`docs/resource-budget.md` § Composed budget): worst-case RAM = OS floor ~494 MiB + both caps (64+96 MiB) ≈ 654 of 954 MiB (~300 MiB headroom); disk is growth-rate-bound (25 GiB free; data 1.2 MB, backup sets 2.7 MB, journal 24 MB at first sample). A periodic sampler (`dg-resource-sample.timer`, installed 2026-10-19 with owner approval) now records the trend; first live sample: gateway 10.5 MiB, sidecar 48.6 MiB (both under the half-/two-thirds-cap thresholds), MemAvailable 460 MiB. Verdict attempted 2026-11-03 on a 14.extended / 61-sample series: sidecar, memory floor and disk slope pass (16.2% of free over 90 days), but one gateway sample reads 55.0 MiB against the 32 MiB every-sample ceiling, so the verdict stays open pending clean samples under the post-upgrade binary (`docs/resource-budget.md` sampler section). No final 1 GB fit claim until then. |
| 6 | Liveness vs readiness | **Ready and exercised** | Liveness remains unauthenticated `GET /health/live` → `{"status":"ok"}` (deliberately not DB/backup health, `src/lib.rs`). The recorded service was active, survived restart, and passed authenticated known-row smoke and restore queries. No readiness endpoint exists — do not mistake `/health/live` for one. |
| 7 | Recovery rehearsal | **Ready and exercised, incl. continuous backup and a clean-host restore** | off-host storage path verified live 12/12 (sandbox packet record); continuous off-host replication live on deployment-host since 2026-10-15 (replication record) alongside periodic verified snapshot sets; restores proven exact (packet gates 5/11, sampling record restore 446 ms, sync-drill live scratch restore); failure visibility via backup status/digest watchdog. The full host-loss path was re-proven on a clean replacement host: host A destroyed, host B restored from the off-host artifact with the original credential, integrity and foreign keys clean, and a corrupt artifact publishing nothing (clean-host rehearsal record). Recovery objectives remain a working target, not an accepted production guarantee. |
| 8 | Operator rollback decisions | **Decided 2026-10-19** | Operator decisions recorded in `docs/upgrade-rollback.md` § Operator decisions: extended post-confirmation verification for pre-upgrade snapshots, newest-24 periodic sets retention, operator key in password manager + `/etc/dg/owner.key` only, write-loss window accepted as working target. |

## Verdict

**The supervised non-production deployment is evidenced and operationally
verified — periodic snapshots, continuous off-host replication, and the pilot
launched 2026-10-16 with the first 14 periodic entries recorded through
2026-10-16T21:17Z — and the
first-deploy and
disaster-recovery paths are now proven on a genuinely clean host. But
production readiness is not accepted.** Gates 1–4, 6 and 7 are
verified; gate 8's operator decisions were recorded 2026-10-19; gate 5
is now composed from measured evidence with the multi-day trend
sampling live — its final fit verdict waits for the ≥extended series.
**The pilot verification is not one of the verified items:** its plan calls for 7
green days, the committed pilot log holds 14
periodic entries ending 2026-10-16T21:17Z, and the interval after that has
not been collected into it. Treat the verification gate as open, not passed. No
production migration is implied by any of this evidence.

## Future authorization boundary

The first supervised deploy was authorized separately and is recorded in
the local deployment record. This checklist does
not authorize a future upgrade, migration, production-data operation, or new
host-side component. A future live-change request must explicitly approve the
specific window and action after the remaining resource, recovery, and rollback
requirements are resolved; `docs/deployment.md` remains procedure, not
approval.
