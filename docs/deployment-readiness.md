# micro2 deployment acceptance checklist

Reconciled to the supervised micro2 deployment record in
[`docs/first-deploy-micro2.md`](first-deploy-micro2.md) and its verbatim host
evidence log. The first non-production smoke/restore deployment was explicitly
owner-approved and is evidence of what was actually installed and exercised;
it is **not** a blanket production-readiness approval. This checklist does not
authorize any future host change, upgrade, migration, backup/TLS/monitoring
installation, or production-data action.

## Gates

| # | Gate | Status | Evidence / unknown |
|---|---|---|---|
| 1 | Architecture fit | **Verified for supervised deploy** | The recorded target is Ubuntu, x86_64, 2 vCPU, glibc 2.39, with systemd; the micro2 release bundle's ELF machine and `/lib64/ld-linux-x86-64.so.2` interpreter were verified on-host. See [`first-deploy-micro2.md`](first-deploy-micro2.md). |
| 2 | Persistent disk | **Verified for current layout; growth unmeasured** | `/var/lib/dg` is `dg:dg`, mode `0700`, with 25 GiB free during the recorded drill; restart persistence was verified. Long-term growth, reboot persistence, and backup-sidecar disk use remain unmeasured. |
| 3 | Network / TLS or private access | **Accepted: SSH tunnel** | `dg serve` speaks plain HTTP on loopback only and refuses non-loopback binds. Remote CLI use goes through `ssh -L 7432:127.0.0.1:7432 <host>`, then `dg --server http://127.0.0.1:7432 --key-file <0600 key>`. TLS-terminating proxy and mesh VPN remain future options; there is no TLS in the gateway. |
| 4 | Service isolation | **Ready and verified on micro2** | `deploy/dg.service` passed `scripts/validate-deploy.sh`; the recorded service uses a dedicated `dg` user, `ProtectSystem=strict`, `NoNewPrivileges`, a writable data dir only, and no secrets in the unit. It is active and enabled on the recorded host. |
| 5 | Measured resource budget | **Not ready** | `docs/resource-budget.md`: the recorded host sample reports 8292 KiB idle after the drill, but OS baseline, backup sidecar, burst peak, and disk growth are unmeasured; five target-host measurements still block a 1 GB fit claim. |
| 6 | Liveness vs readiness | **Ready and exercised** | Liveness remains unauthenticated `GET /health/live` → `{"status":"ok"}` (deliberately not DB/backup health, `src/lib.rs`). The recorded service was active, survived restart, and passed authenticated known-row smoke and restore queries. No readiness endpoint exists — do not mistake `/health/live` for one. |
| 7 | Recovery rehearsal | **Partial; host snapshot drill verified** | The recorded micro2 drill exported/imported throwaway databases and verified copy independence; local crash/restart tests and the upgrade/rollback rehearsal also pass. Continuous backup (Litestream → OVH) is configured nowhere and its restore path is unrehearsed. |
| 8 | Operator rollback decisions | **Awaiting owner** | Runbook exists (`docs/upgrade-rollback.md`). Undecided: soak period before deleting pre-upgrade snapshots, snapshot retention count, who holds the owner credential copy, and the post-snapshot write-loss window acceptance. |

## Verdict

**The first supervised non-production deploy is evidenced and operationally
verified, but production readiness is not accepted.** Gates 1–4 and 6 are
verified for the recorded deployment; gate 5 remains not ready, gate 7 lacks
continuous backup, and gate 8 still awaits owner decisions. No production
migration or backup/TLS/monitoring rollout is implied by the smoke/restore
record.

## Future authorization boundary

The first supervised deploy was authorized separately and is recorded in
[`docs/first-deploy-micro2.md`](first-deploy-micro2.md). This checklist does
not authorize a future upgrade, migration, production-data operation, or new
host-side component. A future live-change request must explicitly approve the
specific window and action after the remaining resource, recovery, and rollback
requirements are resolved; `docs/deployment.md` remains procedure, not
approval.
