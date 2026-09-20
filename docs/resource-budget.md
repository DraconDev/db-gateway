# 1 GB deployment budget

Scope: a 1 GiB host (RAM, plus disk headroom) for the gateway, its OS, and its
backup sidecar. This document separates what has been measured from what has
not, and names the measurements that block a release decision. It does not
invent a total.

## Measured evidence

All measurements are local, single-client, loopback, small-workload baselines.
Commands are reproducible; rerun them rather than trusting these numbers.

| # | Evidence | Artifact / command | Result |
|---|---|---|---|
| M1 | Idle + small-workload harness (debug) | `scripts/resource-report.sh`, see `docs/resource-harness.md` | idle RSS 14,792 KiB; loaded (3 DBs x 100 rows) 16,008 KiB; p50 SELECT 3.8 ms; data dir 86,016 bytes |
| M2 | Release idle + 1 DB x 100 rows | `cargo build --release --locked`, manual `dg serve` + `VmRSS` sampling (2026-09-18) | idle RSS 4,260 KiB; loaded 4,276 KiB; data dir 45,056 bytes; release binary 12,571,768 bytes on disk |
| M3 | Concurrency and per-request caps (code-derived bounds) | `src/store.rs:98` (4 global permits), `src/store.rs:269` (1 writer per DB), `src/execution.rs:40,52,65` (one connection per query, no pool), `docs/api.md` limits table | Worst-case concurrent query executions: 4 + maintenance purge; per-request memory bounded by 64 KiB body / 256 KiB or 1000-row results / 8 MiB export |
| M4 | Supervised micro2 idle-after-drill sample | `docs/first-deploy-micro2.md` § Procedure evidence; `docs/first-deploy-micro2-evidence.log` | 8292 KiB gateway `VmRSS` after the smoke, restart, and throwaway export/import drill; one idle sample, not a host-budget acceptance |
| M5 | Local Profile C filesystem rehearsal | `cargo run --locked --bin recovery_rehearsal`; durable output `docs/recovery-rehearsal-evidence.json` | baseline separate-target restore 76 ms; fallback restore 82 ms; WAL held under reader 939,392 bytes; no Litestream or sidecar, so this is not a host-fit measurement |

M2 is a manual spot check, not a harness run: `scripts/resource-report.sh`
builds the debug profile. A release-mode harness run is listed below as a
release-blocking measurement.

## Component ledger

| Component | Status | Basis |
|---|---|---|
| Gateway process RSS, small workload | Measured (M1, M2) | ~4.3 MiB release idle; ~15 MiB debug loaded. Release + realistic data: **unmeasured**. |
| OS baseline | Partially known, workload unmeasured | The supervised target is an approximately 1 GiB Ubuntu micro2 host, but OS baseline, page-cache behavior, and concurrent workload footprint were not measured. |
| SQLite connections / page cache | Bounded by construction, not stress-verified | No pool; ≤4 concurrent query connections + occasional purge connection; SQLite default cache ≈ 2 MiB per connection ⇒ ≈ 10 MiB order-of-magnitude ceiling. Sustained-burst peak RSS **unmeasured**. |
| Backup tooling (Litestream sidecar) | Unmeasured (M5 is not sidecar evidence) | The local filesystem rehearsal validates fault/status/restore invariants, but no sidecar runs locally; Litestream RSS, disk staging, replication bandwidth, and five-minute lag are unknown. See `docs/backup-compatibility.md` for the pinned version only. |
| Burst headroom | Partially bounded | 2 s deadline + 429 capacity rejects cap concurrency (see contention test), but peak RSS under sustained capacity load is **unmeasured** on micro2. |
| Disk growth | Baseline only | 86 KiB for 300 small rows + WAL + metadata. Growth per real workload, tombstone 24 h retention accumulation, and purge cadence effects are **unmeasured**. |

## Release-blocking measurements

No 1 GB fit claim is possible until each of these is measured on the target
host shape (or an equivalent VM), with the release profile:

1. Release-profile RSS under a realistic workload (owner's actual database
   sizes and query mix), idle and sustained.
2. Litestream sidecar RSS + disk staging during a snapshot/replication cycle.
3. Target OS baseline RSS and page-cache behavior with the data dir hot.
4. Burst test: sustained writes at the 429 capacity edge; record peak RSS and
   confirm recovery to idle baseline.
5. Disk growth projection: real data sizes x retention windows, including
   tombstoned databases awaiting purge.

## Conditional readiness conclusion

**Not ready to declare.** The measured gateway process (~4-16 MiB in tiny
workloads, including one 8292 KiB supervised micro2 sample) leaves the 1 GiB
envelope apparently ample, but the OS baseline, backup sidecar, burst peak,
and disk growth — any one of which could dominate — are unmeasured. M5's
millisecond restore result and WAL sample do not close those gaps. Completing
items 1-5 above is the release gate; until then the budget position is "no
evidence of pressure, no evidence of fit".

## Assumptions and required target-host checks

- Assumes the target runs the release binary behind loopback-only serving (or
  an equivalent reverse proxy whose own footprint is then added to the
  ledger — currently absent).
- Assumes SQLite defaults (2 MiB page cache per connection); if the target
  tunes `PRAGMA cache_size`, re-derive the cache row.
- Required before production readiness or another live change: run items 1-5
  with the release profile, record host context (`uname`, `nproc`, `MemTotal`),
  and attach the reports to the deployment checklist. The 8292 KiB sample is a
  target fact for the recorded drill, not a substitute for the full budget.
