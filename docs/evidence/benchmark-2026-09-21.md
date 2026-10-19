# Benchmark F2b (2026-09-21, local debug build)

Method: `/tmp/bench.py` (scratch probe, not committed) drove a real debug
`dg serve` on a temp data dir through three phases. Host: 16 CPUs, 31 GiB
RAM (build host, not the 1 GiB target — absolute numbers do not transfer;
ratios and the bottleneck do). No other load. Single run, not a sample.

## Results

| Phase | Offered | Admitted | Shed (429) | Wall | Rate | p50 | p99 | Peak RSS |
|---|---|---|---|---|---|---|---|---|
| A. Build 50 DBs × 100 rows (sequential batches) | 5,000 rows | 5,000 | 0 | 9.2 s | 545 rows/s | — | — | 17 MB |
| B. Burst: 12 threads × 100 keyed writes, 1 DB, no retry | 1,200 | 39 (3%) | 1,161 | 1.0 s | — | 33 ms* | 66 ms* | 20 MB |
| B2. Sustained: 4 threads × 100 writes, 1 DB, no retry | 400 | 34 (8.5%) | 366 | 0.8 s | 42 writes/s | 5 ms | 41 ms | — |
| C. Reads: 8 threads × 50 point SELECTs, retry on 429 | 400 | 400 | 242 retries | 1.1 s | 376 reads/s | — | — | 20 MB |

\**B latencies cover admitted writes only.*

Zero unexpected errors in all phases: every non-200 was an explicit `429
capacity_exceeded`, every retried read returned exact rows.

## Interpretation

1. **The admission model is the first bottleneck, not memory or SQLite.**
   Both semaphores use `try_acquire` (zero waiting room): 4 global permits
   plus 1 slot per database. Any concurrency past that sheds instantly —
   even 4 threads on 1 database lose 91% (B2), because they collide on the
   single per-DB slot. Naive clients without retry see single-digit success
   under burst.
2. **Reads shed as badly as writes** (C: 38% first-try 429s at 8 threads),
   because reads share the same global permits and per-DB slot. WAL-mode
   SQLite could serve those readers concurrently; the gateway serializes them.
3. **Memory is healthy**: ~20 MB peak RSS across 50 databases, bursts, and
   parallel reads. No evidence of per-request or per-database bloat.
4. **Sequential throughput is fine** (545 rows/s with per-batch HTTP), so the
   engine is not slow — only concurrent admission is.
5. Client guidance until F3 lands: retry `429` with backoff (keyed writes
   for safe replay; reads are always safe to retry). Unkeyed writes under
   burst risk `outcome_unknown` pile-ups — prefer keys.

## Consequence

F3 (scaling proposal) must start at admission: bounded wait queues, separate
read/write lanes, and concurrent readers per database — in that order. See
`docs/scale-proposal-2026-09-21.md`.

## Re-measurement (2026-09-22, post-lanes release build)

Method: `/tmp/bench2.py` (scratch probe, not committed) repeats the same
three phases against `target/release/dg` with Phases 1–2 implemented
(500 ms bounded wait, 8/4 read/write lanes, shared read guard, deferred
read transactions). Same build host (16 CPUs, 31 GiB — ratios transfer to
micro2, absolutes do not). Single run, not a sample.

| Phase | Offered | Admitted | Shed (429) | Wall | Rate | p50 | p99 | Peak RSS |
|---|---|---|---|---|---|---|---|---|
| A. Build 50 DBs × 100 rows (sequential batches) | 5,000 rows | 5,000 | 0 | 6.3 s | 796 rows/s | — | — | 10 MB |
| B. Burst: 12 threads × 100 writes, 1 DB, no retry | 1,200 | 1,200 (100%) | 0 | 5.9 s | — | 48 ms | 141 ms | 10 MB |
| B2. Sustained: 4 threads × 100 writes, 1 DB, no retry | 400 | 400 (100%) | 0 | 2.1 s | 190 writes/s | 16 ms | 68 ms | — |
| C. Reads: 8 threads × 50 point SELECTs, retry on 429 | 400 | 400 | 0 retries | 0.1 s | ~4,000 reads/s | 2 ms | 5 ms | 10 MB |

Zero non-200 responses in any phase. Phase 1 acceptance met (admission
3% → 100%, p99 141 ms ≈ 2× the B2 p99); Phase 2 acceptance met (phase C
retry-free; single-DB write rate 545 → 796 rows/s — note the old figure was
a debug build, so the gain mixes release profile with lanes).

Constants decision (proposal Phase 3): keep `ACQUIRE_TIMEOUT` 500 ms,
`READ_PERMITS`/`WRITE_PERMITS` 8/4, 250 ms busy timeout, 2 s deadline. B
absorbs a 12-thread burst with p99 well inside the timeout and zero shed;
C shows no read contention at all. No measurement here argues for a change;
re-derive only after micro2 (1 GiB, fewer CPUs) numbers exist.

## Validity (added 2026-10-11)

Nothing above is a universal throughput ceiling. Every figure is a
single-run measurement of one workload (the phase table) on one host
(16 CPUs, 31 GiB build host, unloaded, debug or release as noted) under
one durability configuration — and durability is the expensive half of
these numbers:

- `journal_mode=WAL`, `synchronous=FULL` (`src/execution.rs`, `src/store.rs`);
- every request runs in one managed transaction and a write is
  acknowledged only after commit; a failed statement rolls back the
  whole request (`docs/api.md` Write semantics).

Change the host, the concurrency shape, or any durability setting and
the rates and latencies change with them. Reproduce or re-derive on the
target host before quoting a number as a limit.
