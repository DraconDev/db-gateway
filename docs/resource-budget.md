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
| M2r | Release idle + loaded + burst + page-cache, harness | `scripts/resource-report-release.sh`, evidence `docs/evidence/first-resource-report-release-2026-10-10.json` | release idle 8,480 KiB; loaded (3 DBs x 100 rows) 8,944 KiB; peak under 50-way 4 s INSERT burst 13,056 KiB; post-burst settle 11,752 KiB; p50 SELECT 0.85 ms; release binary 14,238,576 bytes; OS hot Cached 2,491,944 KiB (cold 1,354,640; dev-host delta is not the binding term, but the methodology transfers) |
| M3 | Concurrency and per-request caps (code-derived bounds) | `src/store/admission.rs:12-14` (`READ_PERMITS=8`, `WRITE_PERMITS=4`), `src/store/mod.rs:153-154` (the two `Semaphore` lanes), `src/store/admission.rs:277` (`acquire_queue`: many readers share, one writer excludes, per database), `src/execution.rs:39-50` (`open`: one connection per query, no pool, `cache_size=-2048` at :42), `docs/api.md` limits table | Worst-case concurrent query executions: 8 readers + 4 writers across the two admission lanes, plus maintenance purge; per-request memory bounded by 64 KiB body / 256 KiB or 1000-row results / 8 MiB export. Per-process SQLite cache ceiling: 12 connections x 2 MiB = 24 MiB; not the 10 MiB a single-lane reading would imply (the 2026-09-27 audit correction this goal is built on). |
| M4 | Supervised micro2 idle-after-drill sample | `docs/evidence/first-deploy-micro2.md` § Procedure evidence; `docs/evidence/first-deploy-micro2-evidence.log` | 8292 KiB gateway `VmRSS` after the smoke, restart, and throwaway export/import drill; one idle sample, not a host-budget acceptance |
| M5 | Local Profile C filesystem rehearsal | `cargo run --locked --bin recovery_rehearsal`; durable output `docs/evidence/recovery-rehearsal-evidence.json` | baseline separate-target restore 101 ms; fallback restore 92 ms; WAL held under reader 939,392 bytes; no Litestream or sidecar, so this is not a host-fit measurement |
| M6 | micro2 steady-state + restore with raw transcript (2026-10-16) | `scripts/m4-micro2-sampling.sh`; transcript `docs/evidence/m4-micro2-sampling-2026-10-16.log`; prior summary `docs/evidence/m4-micro2-headroom-2026-10-15.md`; `docs/evidence/resource-report-release-1G-scope-2026-10-15.json` | gateway 6696 KiB steady (3 passes), sidecar ~21300 KiB steady / 36240 HWM, host ~432 MiB free of 954, zero error/queue_timeout counters; litestream restore 392–938 ms across runs, 24576 bytes, integrity ok, 5=5 rows; local enforced rerun idle 8236 / burst peak 11192 KiB, p50 1.66 ms, no OOM |
| M7 | Local failure-visibility drill: revoke / egress / disk (2026-10-15) | `scripts/m4-failure-visibility-drill.sh`; evidence `docs/evidence/m4-failure-visibility-2026-10-15.json` (+ `.log`) | revoked key writes 401 `invalid_credential` while owner writes 200; backup-to-unreachable fails exit 1 with `attempts.jsonl` outcome error/step list while writes 200; prlimit ceiling kills writer (SIGXFSZ surface) with acknowledged rows 4→20 surviving restart and post-recovery writes 200; `/v1/backups` reports `stale` (never false fresh) in all three |

M2 is a manual spot check, not a harness run: `scripts/resource-report.sh`
builds the debug profile. A release-mode harness run is listed below as a
release-blocking measurement.

## Component ledger

| Component | Status | Basis |
|---|---|---|
| Gateway process RSS, small workload | Measured (M1, M2) | ~4.3 MiB release idle; ~15 MiB debug loaded. Release + realistic data: **unmeasured**. |
| OS baseline | Partially known, workload unmeasured | The supervised target is an approximately 1 GiB Ubuntu micro2 host, but OS baseline, page-cache behavior, and concurrent workload footprint were not measured. |
| SQLite connections / page cache | Bounded by construction, not stress-verified | No pool; ≤8 reader + ≤4 writer concurrent query connections (the read and write admission lanes are independent) + occasional purge connection; SQLite default cache ≈ 2 MiB per connection ⇒ ≈ 24 MiB order-of-magnitude ceiling, not the ≈ 10 MiB a single 4-permit lane would imply. Sustained-burst peak RSS **unmeasured**. |
| Backup tooling (Litestream sidecar) | Measured locally, 1 GiB enforced, run committed (2026-09-29) | Gate 9 sampled a 6.9 MiB / 120 000-row database through `litestream replicate` and `restore` inside a kernel-enforced `MemoryMax=1G` scope: replication peak 45,632 KiB, restore peak 34,576 KiB, restore 2740 ms, no OOM kill. Evidence: `docs/evidence/sandbox-packet-2026-09-29-rehearsal.log`. See Release-blocking item 2 for the enforcement probe and for what this does NOT cover (different hardware, a week of WAL growth, real disk). |
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
>
> **Set 2026-10-10, revised 2026-10-16:** the owner-supervised `deploy/dg.service` sets
> `MemoryMax=64M`, matching what the micro2 host already enforces (67108864).
> The 2026-10-10 justification was the four-times-the-cache-ceiling
> formula (ceiling from source M3, 24 MiB; gateway burst peak 13 MiB dev-host;
> sidecar peak 44 MiB in its own unit). The 2026-10-16 on-target record
> (`docs/evidence/m4-micro2-sampling-2026-10-16.log`: gateway steady 6696 KiB,
> local enforced burst peak 11192 KiB) supports 64M: ~2.7x the cache ceiling
> and ~6x the measured burst peak. An earlier template revision said 96M
> while the host ran 64M; this revision resolves that drift toward the host
> fact rather than carrying two values.

1. Release-profile RSS under a realistic workload (owner's actual database
   sizes and query mix), idle and sustained.
   *Measured 2026-10-10 on the dev host*:
   `docs/evidence/first-resource-report-release-2026-10-10.json`, reproducible via
   `scripts/resource-report-release.sh`. The harness builds the release
   binary, starts `dg serve`, samples VmRSS at four points (idle, after
   loading 3 DBs x 100 rows, peak under a 50-way 4 s concurrent INSERT
   burst, post-burst settle), and records the OS state (cold and hot
   Cached) so the page-cache growth can be observed alongside the process
   RSS. Numbers, in KiB: idle 8,480; loaded 8,944; peak-under-burst
   13,056; post-burst 11,752. The p50 single-row SELECT latency on the
   release profile is 0.85 ms, ~4x lower than the 3.8 ms the debug
   harness reports. The 13 MiB peak is well below the 24 MiB cache
   ceiling; the ceiling is the binding term because the workload is
   small and the cache only fills under repeated hot queries. Re-run
   on the supervised micro2 host with the same harness to capture the
   on-target fact.
2. Litestream sidecar RSS + disk staging during a snapshot/replication cycle.
   *Measured 2026-09-29 against the rehearsal endpoint with pinned 0.5.17, and
   the run is committed*:
   `docs/evidence/sandbox-packet-2026-09-29-rehearsal.log` (gate 9). The sampler ran a
   6.9 MiB / 120 000-row database through `litestream replicate` for the
   sampling window with one live INSERT per 0.4 s, then a `litestream restore`
   from the bucket. Peaks, as recorded by the packet:

   | measurement | value |
   | --- | --- |
   | constraint | `MemoryMax=1G` systemd transient scope, enforcement proved by an OOM-kill probe |
   | steady replication, peak RSS | 45,632 KiB = 44 MiB (4% of the 1 GiB budget) |
   | restore | 2740 ms, 120 030 rows restored, peak child RSS 34,576 KiB = 33 MiB (3% of budget) |
   | killed for exceeding the budget | no |

   **These numbers are recomputable:** `python3 scripts/packet-facts.py
   docs/evidence/sandbox-packet-2026-09-29-rehearsal.log` prints them from the packet, so
   a reader does not have to trust this table over the artifact it claims to
   describe. **An earlier table in this file quoted 48 000 / 35 000 KiB and then
   42 000 / 34 000 KiB, from two runs that were never committed.** Numbers with no
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
   *Measured 2026-10-10 on the dev host* (methodology transfers; the dev
   host's Cached column is not the binding term at 32 GiB, but the harness
   records the cold/hot delta so the same measurement on micro2 produces a
   comparable number): `docs/evidence/first-resource-report-release-2026-10-10.json`
   `os_memory_kib` — cold Cached 1,354,640 KiB, hot Cached 2,491,944 KiB,
   hot MemAvailable 8,699,572 KiB of 31,914,628 KiB total. The release
   process grows its cache up to the 24 MiB ceiling (item 1) by warming the
   SQLite cache, and the host's Cached column grows to cover the binary,
   the data dir, and the workload. On a 1 GiB host the same workload will
   pin a much larger fraction of available memory; the supervised run on
   micro2 is the place to confirm.
4. Burst test: sustained writes at the 429 capacity edge; record peak RSS and
   confirm recovery to idle baseline.
   *Measured 2026-10-10 on the dev host* (release, 50 concurrent INSERTs for
   4 s): peak 13,056 KiB, post-burst settle 11,752 KiB, ~10% shrink within
   2 s of burst end. The harness does not enforce 429 rejection at the
   capacity edge (it sends as fast as curl will dispatch, which is below
   the 8-read + 4-write admission cap on loopback); the supervised micro2
   run is where 429-rejection behavior is recorded.
5. Disk growth projection: real data sizes x retention windows, including
   tombstoned databases awaiting purge.

## Conditional readiness conclusion

**Steady-idle inputs 1, 2, 3 and the local-burst 4-half are measured as of
2026-10-16 (M6); target-host burst at the 429 edge and disk-growth item 5
remain open, so the budget position is "no claim of fit" (see below).**
The 1 GiB envelope so far is composed of:

- Gateway process RSS: 13 MiB peak under burst (release, dev host) or
  8 MiB idle-after-drill (supervised micro2 sample), re-confirmed as
  6696 KiB steady on micro2 (raw transcript
  `docs/evidence/m4-micro2-sampling-2026-10-16.log`; the one-off sampler that
  produced it was retired 2026-10-19 in A12, preserved in git history)
  and 11 MiB burst peak under a
  1 GiB-enforced local rerun (2026-10-15, M6). Worst-case ceiling
  from code: 24 MiB SQLite cache (8+4 connections x 2 MiB).
- Litestream sidecar: 44 MiB peak replication, 33 MiB restore (1 GiB
  enforced scope, rehearsal packet gate 9); on micro2 steady ~21 MiB /
  35 MiB HWM with a verified restore (392–938 ms across runs,
  integrity ok, 5=5 rows — same transcript).
- OS + page cache: budget remainder (the supervised micro2 run is where
  this is observed under real pressure).

Drift resolved 2026-10-16: template and host agree on `MemoryMax=64M`
for the gateway (the host already enforced 67108864; the template now
says 64M with the on-target measurement as justification). The sidecar
keeps its own 96M cap in its own unit.

`MemoryMax=64M` in `deploy/dg.service` is the gateway cap; the sidecar runs
in its own unit with its own `MemoryMax`. The 64 MiB is ~2.7x the cache
ceiling (the worst case) and ~6x the measured burst peak; it is ~1/15 of
the 1 GiB host and leaves the OS + sidecar with the remaining ~890 MiB.

Item 5 (disk growth projection) and the target-host item-4 burst (429
capacity-reject regime) are not yet measured. The local rehearsal is
not a substitute for the target-host measurement, and the budget
position remains "no claim of fit" until the supervised micro2 run
with this same harness records the on-target fact.

## Composed budget (2026-10-19, audit A7)

Composing the measured pieces against the 1 GiB envelope (micro2:
954 MiB total, M6) — this is the composition gate 5 was missing; the
multi-day *trend* confirmation is the sampler series below.

**RAM.** At measured steady state the host used ≈522 MiB of 954
(M6: ~432 MiB free) *including* the gateway (6.7 MiB) and sidecar
(~21 MiB). Composed worst case is therefore not steady + caps
(that double-counts), it is: OS-and-everything-else floor
≈522 − 7 − 21 ≈ 494 MiB, plus both caps at their ceilings
(64 MiB gateway + 96 MiB sidecar) ≈ **654 MiB of 954 MiB**, leaving
≈300 MiB headroom even if both processes simultaneously hit caps
they have never approached (gateway burst peak 13 MiB, sidecar
rehearsal peak 44.6 MiB). RAM fits with margin; it is not the
binding term.

**Disk.** 25 GiB free (readiness gate 2). Local consumers compose
as: data dir D (live DBs + registry + WAL), backup sets ≈
`--keep 24` × D_snapshot (each set carries every database file;
D today is single-digit MiB, so sets are tens of MiB total), one
tombstone overlap (≤ D for 24 h), journal (journald-bounded;
measured by the sampler, retention policy is audit A8), release
binary ≈14 MiB. Even a 100× data growth keeps local disk in the
hundreds of MiB against 25 GiB. The binding disk term is *growth
rate*, not today's bytes — which is exactly what the series fits.

**The sampler series (closes the trend half of gate 5).**
`scripts/resource-sample.sh` (timer template
`deploy/dg-resource-sample.{service,timer}`, 6-hourly) appends one
JSON sample per fire: per-unit MemoryCurrent/Peak, data + backups
dir bytes, WAL bytes, journal bytes, host MemAvailable and disk.
Gate 5's trend claim passes when a ≥7-day series shows: gateway
MemoryCurrent < 32 MiB and sidecar < 64 MiB at every sample
(half-cap ceilings), MemAvailable never < 128 MiB, and the
least-squares slope of (data + backups + journal) bytes projects
to < 50% of the free disk over 90 days. Installing the timer is a
live change and needs owner approval; until the series exists the
verdict stays "steady-state composed, trend unproven" — an honest
statement, not a fit claim.

**Verdict attempt 2026-11-03 (series 2026-10-19 → 2026-11-03, 61
samples, 14.7 days): three of four criteria pass.** Sidecar max
50.0 MiB (ceiling 64), MemAvailable min 302 MiB (floor 128), and
the growth slope +38.9 MiB/day projects to 16.2% of free disk over
90 days (limit 50%) — the journal dominates the slope (27.8 →
512.3 MiB total; data + backups stay under 17 MiB). The gateway
criterion fails on one sample: 60 of 61 under 32 MiB (peaks ≤27.3
MiB, no leak trajectory), but the final sample reads 55.0 MiB with
no error in any log and a normal 2-second backup in the same
second. A post-upgrade backup-time experiment moves gateway memory
21.2 → 26.6 MiB only, so the spike is not a reproducible
backup artifact. The spiking process is gone (binary upgraded and
service restarted 2026-11-03T10:37Z); the gate stays open pending
clean samples under the new binary. Note for the next attempt:
the 6-hourly sampler fires at :17, the same minute as the hourly
backup, so every sample is backup-loaded — pessimistic, hence
conservative for this gate, but noisy; do not retime it until the
verdict is recorded, or the regimes stop being comparable.

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
