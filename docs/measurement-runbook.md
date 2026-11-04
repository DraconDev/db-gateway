# deployment-host measurement runbook (RUN 2026-09-22 — see docs/evidence/deployment-host-2026-09-22-evidence.log; verification adapted to 600 s per runbook, burst rerun unkeyed after keyed run hit the replay cap by design)

Exact steps for resource-budget items 1–5 (`docs/resource-budget.md`), the
burst test (tasklist D2), and log-retention setup (R11). Procedure only —
running it needs private transport access to deployment-host plus a separately approved live window.
All commands are read-only except where marked **WRITE**; WRITE steps run
only inside the approved window against the test instance/data, never
production data (none exists yet).

Record host context once per session and attach it to every report:

```sh
uname -a; nproc; free -m; df -h / /var/lib/dg; cat /proc/meminfo | head -3
systemctl show dg.service -p MemoryMax,ActiveState,SubState  # expect MemoryMax=64M (67108864, shipped 2026-10-16)
```

## D1. Release-profile RSS under realistic workload

```sh
# Steady workload: 6 fleet-shaped DBs seeded from fleet/*.sql via dg, then
# 10 minutes of mixed reads/writes (adapt scripts/verification.py --seconds 600).
# Sample RSS every 10 s:
for i in $(seq 1 60); do
  grep VmRSS /proc/$(systemctl show dg.service -p MainPID --value)/status
  sleep 10
done | tee rss-steady.log
```

Record: idle RSS, steady-state mean/p95/max, workload description. This plus
D2 sets `MemoryMax` via the R10 formula.

## D2. Burst test at the 429 edge

Drive concurrent writers past the 4-permit global semaphore until 429s flow
(12 threads × keyed single-row INSERTs works — see `tests/key_chaos.rs` for
the shape, or run that test binary against the host with a test key — **WRITE**,
test instance only). Hold the burst 5 minutes, then stop and watch recovery.

Record: peak RSS during burst, 429 rate, time to return to idle baseline,
zero 500s / zero lost acknowledged writes (replay unknown-outcome keys and
verify exact rows).

## D3a. Target OS baseline

```sh
free -m; vmstat 1 30 > vmstat-baseline.txt          # idle, service stopped
sudo systemctl stop dg.service                      # **WRITE** (test instance)
sleep 30; free -m                                   # OS-only baseline
sudo systemctl start dg.service
```

Record: OS-only MemAvailable vs with-service; page-cache behavior with the
data dir hot (`vmtouch -v /var/lib/dg || du -sh /var/lib/dg` + `free -m`
before/after a full-table scan of every DB).

## D3b. Sidecar footprint (after C1-live)

During steady replication and during a restore, sample the replication sidecar
process RSS + CPU the same way as D1, plus replication bandwidth (`nload` or
`/proc/net/dev` deltas) and disk staging under the sidecar's work dir.

## D4-disk. Growth projection (resource-budget item 5)

```sh
du -sb /var/lib/dg; ls -la /var/lib/dg | head -30
# Per-database sizes + WAL sidecars:
for f in /var/lib/dg/*.sqlite; do du -sb "$f"* 2>/dev/null; done
```

Combine with expected row growth per application to project: live data ×
retention windows + tombstoned accumulation over 24 h + transfer staging
(≤ 8 × 128 MiB worst case, TTL'd). Record the projection table; it gates the
disk-full drill and any quota choice.

## R11. Log retention

```sh
# **WRITE**: cap the host journal so JSON stderr cannot fill the disk.
sudo mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=500M\nMaxRetentionSec=30day\n' |
  sudo tee /etc/systemd/journald.conf.d/dg.conf
sudo systemctl restart systemd-journald
journalctl --disk-usage   # record
```

## Close-out

Attach to `docs/deployment-readiness.md` gate 5: host context, rss-steady.log,
burst report, baseline numbers, growth projection, journal usage. Set
`MemoryMax` in `deploy/dg.service` from R10's formula in the same change that
records the numbers — never from memory.
