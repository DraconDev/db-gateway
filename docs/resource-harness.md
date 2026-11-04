# Resource-measurement harness

Two documented commands produce machine-readable local baselines:

```
scripts/resource-report.sh             # debug profile
scripts/resource-report-release.sh     # release profile
```

The debug script builds the debug binary (`cargo build --locked`); the release
script builds the release binary (`cargo build --release --locked`). Both
start a real `dg serve` on a disposable directory, sample VmRSS, load a
fixed workload, sample local latency, print one JSON report to stdout, then
kill the server and remove the workdir (trap on exit). The release script
adds a 50-way concurrent INSERT burst (4 s window) and OS cold/hot
`/proc/meminfo` snapshots so the page-cache growth attributable to the
workload is observable.

## Fixed workload

- 3 databases (`w1..w3`), each with one table `kv(k TEXT PRIMARY KEY, v TEXT)`
  holding 100 small rows (`k0..k99`).
- 20 sequential single-row `SELECT v FROM kv WHERE k = 'k42'` probes against
  `w2`, timed with curl `time_total`; report carries min/p50/max.
- Memory is process RSS (`VmRSS`) sampled idle and after loading.
- Disk is `du -sb` of the data directory (database files + WAL + metadata).
- The release script also runs a 50-way 4 s concurrent INSERT burst on `w1`
  and samples VmRSS at 100 ms intervals to capture the peak under load,
  then samples again 2 s after the burst ends to confirm the process
  shrinks back toward idle.

## Sample debug run (2026-09-18, dev host)

Host: Linux 7.0.9 x86_64, 16 CPUs, 32 GiB RAM. Debug build.

| Field | Value |
|---|---|
| idle RSS | 14,792 KiB (~14.4 MiB) |
| loaded RSS | 16,008 KiB (~15.6 MiB) |
| latency min / p50 / max | 2.8 / 3.8 / 10.6 ms |
| disk | 86,016 bytes |

## Sample release run (2026-10-10, dev host)

Host: Linux 7.1.2 x86_64, 16 CPUs, 32 GiB RAM. Release build.

| Field | Value |
|---|---|
| idle RSS | 8,480 KiB (~8.3 MiB) |
| loaded RSS | 8,944 KiB (~8.7 MiB) |
| peak RSS under burst | 13,056 KiB (~12.8 MiB) |
| post-burst settle (2 s) | 11,752 KiB (~11.5 MiB) |
| latency min / p50 / max | 0.7 / 0.85 / 4.4 ms |
| hot Cached (kernel) | 2,491,944 KiB (~2.4 GiB) |
| suggested `MemoryMax=` | 91,264 KiB (~89 MiB) |

The full report is committed as
`docs/evidence/first-resource-report-release-2026-10-10.json`; the report's
`suggested_memory_max_kib` is the largest measured steady-state term
(gateway burst peak vs SQLite cache ceiling vs sidecar peak) doubled
for headroom, with a 64 MiB floor. The unit file `deploy/dg.service`
sets `MemoryMax=64M` — the on-host value the owner decision was
revised to on 2026-10-16, matching what the supervised deployment-host host
enforces (67108864); the supervised deployment-host
run is the place to record the on-target fact under the same cap.

## What is NOT measured

- Continuous-backup sidecars (replication sidecar/object storage provider), OS page cache (debug
  harness), WAL checkpoint timing, TLS/reverse-proxy overhead — none run
  during the debug harness. The release harness samples OS page cache
  cold/hot; the dev host's Cached column is not the binding term at
  32 GiB, but the methodology transfers to a 1 GiB target.
- Contention or multi-client load; local single-client probes only.
- A single run is not a statistical sample.

## Budget position

**Neither report is a claim of compliance with any 1 GB production
budget.** Both are local baselines for trend comparison only (e.g.
re-run after a storage or dependency change and diff the RSS/latency
fields). A production budget claim would require the deployment host,
realistic load, and the replication sidecar running under the
`MemoryMax=1G` envelope documented in
`docs/evidence/sandbox-2026-09-29-rehearsal.log` gate 9. The release
script's `suggested_memory_max_kib` (91,264 KiB, ~89 MiB) is the measured
starting ceiling this discussion starts from; the unit's own cap is
`MemoryMax=64M` (67108864 bytes), and the supervised deployment-host run is the
on-target confirmation.
