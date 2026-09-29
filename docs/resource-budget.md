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
| M3 | Concurrency and per-request caps (code-derived bounds) | `src/store.rs:36-37` (`READ_PERMITS=8`, `WRITE_PERMITS=4`), `src/store.rs:559-560` (the two lanes), `src/store.rs:937` (`acquire_queue`: many readers share, one writer excludes, per database), `src/execution.rs:39-50` (`open`: one connection per query, no pool, `cache_size=-2048` at :42), `docs/api.md` limits table | Worst-case concurrent query executions: 8 readers + 4 writers across the two admission lanes, plus maintenance purge; per-request memory bounded by 64 KiB body / 256 KiB or 1000-row results / 8 MiB export |
| M4 | Supervised micro2 idle-after-drill sample | `docs/first-deploy-micro2.md` § Procedure evidence; `docs/first-deploy-micro2-evidence.log` | 8292 KiB gateway `VmRSS` after the smoke, restart, and throwaway export/import drill; one idle sample, not a host-budget acceptance |
| M5 | Local Profile C filesystem rehearsal | `cargo run --locked --bin recovery_rehearsal`; durable output `docs/recovery-rehearsal-evidence.json` | baseline separate-target restore 101 ms; fallback restore 92 ms; WAL held under reader 939,392 bytes; no Litestream or sidecar, so this is not a host-fit measurement |

M2 is a manual spot check, not a harness run: `scripts/resource-report.sh`
builds the debug profile. A release-mode harness run is listed below as a
release-blocking measurement.

## Component ledger

| Component | Status | Basis |
|---|---|---|
| Gateway process RSS, small workload | Measured (M1, M2) | ~4.3 MiB release idle; ~15 MiB debug loaded. Release + realistic data: **unmeasured**. |
| OS baseline | Partially known, workload unmeasured | The supervised target is an approximately 1 GiB Ubuntu micro2 host, but OS baseline, page-cache behavior, and concurrent workload footprint were not measured. |
| SQLite connections / page cache | Bounded by construction, not stress-verified | No pool; ≤8 reader + ≤4 writer concurrent query connections (the read and write admission lanes are independent) + occasional purge connection; SQLite default cache ≈ 2 MiB per connection ⇒ ≈ 24 MiB order-of-magnitude ceiling, not the ≈ 10 MiB a single 4-permit lane would imply. Sustained-burst peak RSS **unmeasured**. |
| Backup tooling (Litestream sidecar) | Measured locally, 1 GiB enforced, run committed (2026-09-29) | Gate 9 sampled a 6.9 MiB / 120 000-row database through `litestream replicate` and `restore` inside a kernel-enforced `MemoryMax=1G` scope: replication peak 49 652 KiB, restore peak 35 340 KiB, restore 1798 ms, no OOM kill. Evidence: `docs/sandbox-packet-2026-09-29-rehearsal.log`. See Release-blocking item 2 for the enforcement probe and for what this does NOT cover (different hardware, a week of WAL growth, real disk). |
| Burst headroom | Partially bounded | 2 s deadline + 429 capacity rejects cap concurrency (see contention test), but peak RSS under sustained capacity load is **unmeasured** on micro2. |
| Disk growth | Baseline only | 86 KiB for 300 small rows + WAL + metadata. Growth per real workload, tombstone 24 h retention accumulation, and purge cadence effects are **unmeasured**. |

## Release-blocking measurements

No 1 GB fit claim is possible until each of these is measured on the target
host shape (or an equivalent VM), with the release profile:

> **Owner decision 2026-09-27:** `deploy/dg.service` keeps `MemoryMax` unset
> until items 1 and 3 below are measured. A cap chosen without a measurement is
> a guess, and a wrong hard cap gets the gateway OOM-killed under the very
> burst the budget exists to absorb. Item 1 plus item 3, with item 2's sidecar
> headroom, is what would justify a value. Remember the page-cache term in this
> budget is ~24 MiB (8 readers + 4 writers x ~2 MiB), not the ~10 MiB an
> earlier single-lane reading implied.

1. Release-profile RSS under a realistic workload (owner's actual database
   sizes and query mix), idle and sustained.
2. Litestream sidecar RSS + disk staging during a snapshot/replication cycle.
   *Measured 2026-09-29 against the rehearsal endpoint with pinned 0.5.17, and
   the run is committed*:
   `docs/sandbox-packet-2026-09-29-rehearsal.log` (gate 9). The sampler ran a
   6.9 MiB / 120 000-row database through `litestream replicate` for the
   sampling window with one live INSERT per 0.4 s, then a `litestream restore`
   from the bucket. Peaks, as recorded by the packet:

   | measurement | value |
   | --- | --- |
   | constraint | `MemoryMax=1G` systemd transient scope, enforcement proved by an OOM-kill probe |
   | steady replication, peak RSS | 49 652 KiB (4% of the 1 GiB budget) |
   | restore | 1798 ms, 120 030 rows restored, peak child RSS 35 340 KiB (3% of budget) |
   | killed for exceeding the budget | no |

   **An earlier table in this file quoted 48 000 / 35 000 KiB and then 42 000 /
   34 000 KiB, from two runs that were never committed.** Numbers with no
   artifact behind them cannot be audited, and a reader comparing them against
   a committed packet would have had two answers and no way to tell which was
   current. The row above is the one the repository can support; if a later run
   supersedes it, commit that packet with it.

   **The earlier "unconstrained" row was the packet's own defect, not a fact
   about the host.** Gate 9 decided whether the limit was in force by asking
   `/sys/fs/cgroup/cgroup.procs` whether cgroups were usable; that file is
   root-owned and never writable by a user, so the test was always false and
   the gate skipped its constrained path on a machine that could enforce one.
   The question worth asking is whether the CALLER's own cgroup can hold a
   constrained child. It can, by two routes, and both are probed for real:
   `systemd-run --user --scope -p MemoryMax=1G`, and a direct cgroup v2 child
   where the memory controller is delegated to the caller's subtree. The packet
   records which one it used and why the other was rejected.

   **Setting `memory.max` is not enforcing it**, so neither route is accepted
   on the strength of "the file exists and the number was written". The probe
   asks the kernel: it tries to allocate 8x the cap (512 MiB under a 64 MiB
   cap) and requires the child to DIE. A mechanism that lets that complete
   prints `SURVIVED` and is rejected outright, because it would otherwise
   report a "constrained" measurement made under no constraint at all. Under
   the enforced limit the sampled peaks sit at ~4% of budget for replication
   and ~3% for restore, with no OOM kill, so the sidecar is not the binding
   term in this envelope **at this database size**. The 1 GB VPS is still
   where item 2 is properly measured: different hardware, different kernel,
   a real disk, and a week of WAL growth rather than twelve seconds of it.
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
