# micro2 deployment acceptance checklist

Prepared without contacting the host. Each gate maps to evidence or an
explicit unknown. **This document is not permission to deploy** — the
authorization request at the end is the only path forward, and deployment
instructions elsewhere (e.g. `docs/deployment.md`) describe procedure, not
approval.

## Gates

| # | Gate | Status | Evidence / unknown |
|---|---|---|---|
| 1 | Architecture fit | **Unknown** | Release bundle is `x86_64` Linux (`scripts/release-bundle.sh`, `BUILD-INFO.txt`). micro2 CPU arch, OS, and systemd availability are unknown — no host access was used. |
| 2 | Persistent disk | **Unknown** | Data dir needs a persistent, `0700`, service-user-owned path (`docs/deployment.md`). micro2 disk layout, size, and persistence across reboots are unknown. Disk growth rate is unmeasured (`docs/resource-budget.md`). |
| 3 | Network / TLS or private access | **Accepted: SSH tunnel** | `dg serve` speaks plain HTTP on loopback only and refuses non-loopback binds (unchanged — no gateway or CLI modifications). Remote CLI use goes through `ssh -L 7432:127.0.0.1:7432 <host>`, then `dg --server http://127.0.0.1:7432 --key-file <0600 key>`. TLS-terminating proxy and mesh VPN stay documented future upgrades on evidence of friction, not current requirements. No TLS in the gateway. |
| 4 | Service isolation | Ready (template) | `deploy/dg.service` validated by `scripts/validate-deploy.sh` (systemd-analyze clean): dedicated user, `ProtectSystem=strict`, `NoNewPrivileges`, writable data dir only, no secrets in the unit. Not installed anywhere. |
| 5 | Measured resource budget | **Not ready** | `docs/resource-budget.md`: release idle ~4.2 MiB locally, but OS baseline, backup sidecar, burst peak, and disk growth are unmeasured; five target-host measurements block a 1 GB fit claim. |
| 6 | Liveness vs readiness | Ready (defined) | Liveness: unauthenticated `GET /health/live` → `{"status":"ok"}` (deliberately not DB/backup health, `src/lib.rs`). Readiness: the JSON `listening` log line with the bound address (the signal every test harness waits on) plus one authenticated known-row query. No readiness endpoint exists — do not mistake `/health/live` for one. |
| 7 | Recovery rehearsal | Partial | Manual snapshot drill (`tests/recovery_drill.rs`), crash/restart tests, and the upgrade/rollback rehearsal (`docs/upgrade-rollback.md`) all pass locally. Continuous backup (Litestream → OVH) is configured nowhere and its restore path is unrehearsed. |
| 8 | Operator rollback decisions | **Awaiting owner** | Runbook exists (`docs/upgrade-rollback.md`). Undecided: soak period before deleting pre-upgrade snapshots, snapshot retention count, who holds the owner credential copy, and the post-snapshot write-loss window acceptance. |

## Verdict

**Not accepted for deployment.** Gates 1, 2, 5 are unknown/unmeasured and
gates 3, 8 await owner decisions. Gate 4 and 6 are ready as artifacts only.

## Authorization request

When the owner wants to proceed, the request to approve is precisely:

> Approve (a) answering the architecture/disk/network questions in gates
> 1–3 (by providing micro2 specs or access for a read-only survey), (b) the
> five target-host measurements in `docs/resource-budget.md`, (c) the
> rollback decisions in gate 8, and (d) a first supervised deploy window
> following `docs/deployment.md` with the `docs/upgrade-rollback.md`
> snapshot steps. Until all four are explicitly approved, no host is
> contacted and no service is installed.
