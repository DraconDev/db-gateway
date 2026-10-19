# micro2 deployment acceptance checklist

Reconciled to the supervised micro2 deployment record in
[`docs/first-deploy-micro2.md`](first-deploy-micro2.md) and its verbatim host
evidence log. The first non-production smoke/restore deployment was explicitly
owner-approved and is evidence of what was actually installed and exercised;
it is **not** a blanket production-readiness approval. This checklist does not
authorize any future host change, upgrade, migration, backup/TLS/monitoring
installation, or production-data action.

_Last reconciled 2026-10-19 against the Oct 13–19 evidence (live OVH
packet, Litestream sidecar, micro2 sampling, pilot soak, backup
watchdog, replication-coverage sync — see gates 5 and 7). Findings and
remaining work are tracked in `docs/audit-2026-10-19.md`._

## Gates

| # | Gate | Status | Evidence / unknown |
|---|---|---|---|
| 1 | Architecture fit | **Verified for supervised deploy** | The recorded target is Ubuntu, x86_64, 2 vCPU, glibc 2.39, with systemd; the micro2 release bundle's ELF machine and `/lib64/ld-linux-x86-64.so.2` interpreter were verified on-host. See [`first-deploy-micro2.md`](first-deploy-micro2.md). |
| 2 | Persistent disk | **Verified for current layout; growth unmeasured** | `/var/lib/dg` is `dg:dg`, mode `0700`, with 25 GiB free during the recorded drill; restart persistence was verified. Long-term growth, reboot persistence, and backup-sidecar disk use remain unmeasured. |
| 3 | Network / TLS or private access | **Accepted: SSH tunnel** | `dg serve` speaks plain HTTP on loopback only and refuses non-loopback binds. Remote CLI use goes through `ssh -L 7432:127.0.0.1:7432 <host>`, then `dg --server http://127.0.0.1:7432 --key-file <0600 key>`. TLS-terminating proxy and mesh VPN remain future options; there is no TLS in the gateway. |
| 4 | Service isolation | **Ready and verified on micro2** | `deploy/dg.service` passed `scripts/validate-deploy.sh`; the recorded service uses a dedicated `dg` user, `ProtectSystem=strict`, `NoNewPrivileges`, a writable data dir only, and no secrets in the unit. It is active and enabled on the recorded host. |
| 5 | Measured resource budget | **Steady-state measured; composed long-term budget open** | Scripted 3-pass micro2 sampling (`docs/m4-micro2-sampling-2026-10-16.log`): gateway 6,696 KiB steady, sidecar ~21,300 KiB, restore 446 ms integrity-ok — far inside the 96M+96M caps. Local 1G-scope composed evidence also exists (`docs/resource-budget.md`). Still open (audit A7): multi-day composed gateway+sidecar+backup+OS trend, burst peak on the host, and disk/journal growth rates — so no final 1 GB fit claim yet. |
| 6 | Liveness vs readiness | **Ready and exercised** | Liveness remains unauthenticated `GET /health/live` → `{"status":"ok"}` (deliberately not DB/backup health, `src/lib.rs`). The recorded service was active, survived restart, and passed authenticated known-row smoke and restore queries. No readiness endpoint exists — do not mistake `/health/live` for one. |
| 7 | Recovery rehearsal | **Ready and exercised, incl. continuous backup** | OVH path verified live 12/12 (`docs/sandbox-packet-2026-10-14-ovh.log`); Litestream→OVH sidecar live on micro2 since 2026-10-15 (`docs/litestream-replication-evidence.log`) alongside hourly verified snapshot sets; bucket restores proven exact (packet gates 5/11, `docs/m4-micro2-sampling-2026-10-16.log` restore 446 ms, `docs/litestream-sync-drill-2026-10-19.log` live scratch restore); failure visibility via backup status/digest watchdog (`docs/watchdog-2026-10-17.log`). Profile C RPO/RTO remain a working target, not an accepted production guarantee. |
| 8 | Operator rollback decisions | **Awaiting owner** | Runbook exists (`docs/upgrade-rollback.md`). Undecided: soak period before deleting pre-upgrade snapshots, snapshot retention count, who holds the owner credential copy, and the post-snapshot write-loss window acceptance. |

## Verdict

**The supervised non-production deployment is evidenced and operationally
verified — hourly snapshots, continuous OVH replication, and a pilot
soak (since 2026-10-16) included — but production readiness is not
accepted.** Gates 1–4, 6 and 7 are verified; gate 5 has steady-state
measurements but still lacks the composed multi-day budget (audit A7),
and gate 8 still awaits owner decisions. No production migration is
implied by any of this evidence.

## Future authorization boundary

The first supervised deploy was authorized separately and is recorded in
[`docs/first-deploy-micro2.md`](first-deploy-micro2.md). This checklist does
not authorize a future upgrade, migration, production-data operation, or new
host-side component. A future live-change request must explicitly approve the
specific window and action after the remaining resource, recovery, and rollback
requirements are resolved; `docs/deployment.md` remains procedure, not
approval.
