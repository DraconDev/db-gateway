# Resource-measurement harness

One documented command produces a machine-readable local baseline:

```
scripts/resource-report.sh
```

The script builds the debug binary (`cargo build --locked`), starts a real
`dg serve` on a disposable directory, measures idle RSS after a 2 s settle,
loads a fixed workload, samples loopback latency, prints one JSON report to
stdout, then kills the server and removes the workdir (trap on exit).

## Fixed workload

- 3 databases (`w1..w3`), each with one table `kv(k TEXT PRIMARY KEY, v TEXT)`
  holding 100 small rows (`k0..k99`).
- 20 sequential single-row `SELECT v FROM kv WHERE k = 'k42'` probes against
  `w2`, timed with curl `time_total`; report carries min/p50/max.
- Memory is process RSS (`VmRSS`) sampled idle and after loading.
- Disk is `du -sb` of the data directory (database files + WAL + metadata).

## Sample run (2026-09-18, dev host)

Host: Linux 7.0.9 x86_64, 16 CPUs, 32 GiB RAM. Debug build.

| Field | Value |
|---|---|
| idle RSS | 14,792 KiB (~14.4 MiB) |
| loaded RSS | 16,008 KiB (~15.6 MiB) |
| latency min / p50 / max | 2.8 / 3.8 / 10.6 ms |
| disk | 86,016 bytes |

## What is NOT measured

- Continuous-backup sidecars (Litestream/OVH), OS page cache, WAL checkpoint
  timing, TLS/reverse-proxy overhead — none run during the harness.
- Contention or multi-client load; loopback single-client probes only.
- Release-profile behavior (harness builds debug).
- A single run is not a statistical sample.

## Budget position

**This report is not a claim of compliance with any 1 GB production budget.**
It is a local baseline for trend comparison only (e.g. re-run after a storage
or dependency change and diff the RSS/latency fields). A production budget
claim would require the deployment host, release profile, backup stack, and
realistic load — none of which this harness provides.
