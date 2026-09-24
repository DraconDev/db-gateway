# Scaling proposal v1 (2026-09-21, Phases 1–2 implemented)

**Document role:** Implemented scale reference
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)

Status: **Phases 1–2 implemented 2026-09-22; measured sizing and the multi-host
decision remain open.** Evidence: `docs/benchmark-2026-09-21.md` (F2b). This
document authorizes no code change — each remaining phase becomes a task with
its own tests and before/after benchmark when approved.

## Implementation status (2026-09-22)

- Phase 1 landed: `ACQUIRE_TIMEOUT` (500 ms) bounded wait on the global
  permit (`src/api.rs` `work_in`) and the per-database slot
  (`Store::acquire_queue`, poll loop — the blocking thread cannot await).
  Regression: `tests/admission.rs` `burst_waits_instead_of_shedding`
  (fails pre-fix, passes post-fix).
- Phase 2 landed: `READ_PERMITS`/`WRITE_PERMITS` (8/4) lanes, per-database
  `RwLock` guards (`QueueGuard`), syntactic `is_read_only` lane classifier
  (`src/execution.rs`, admission-only — enforcement stays in the authorizer).
  `m2_lifecycle.rs` contention test re-targeted to write-vs-write (reads
  intentionally no longer shed). Regressions: `eight_concurrent_reads`,
  `reads_proceed_during_slow_write`, `lane_classifier_marks_only_reads`.
- Extra finding Phase 2 exposed: owner-keyed SELECTs ran in `Immediate`
  transactions (RESERVED lock for pure reads), shedding concurrent readers
  with SQLITE_BUSY once the shared guard let them through. Statement-
  classified reads now use `Deferred`; writers keep `Immediate`.
- Still open: Phase 3 measured sizing + benchmark table; Phase 4 multi-host
  gate; writer-preferring bound only if starvation is measured.

## Problem statement

F2b proved the gateway's first bottleneck is admission, not memory (~20 MB
peak) or SQLite (545 rows/s sequential). Both semaphores use `try_acquire`
with zero waiting room: 4 global permits + 1 slot per database, shared by
reads and writes. Measured effect: 12 threads on one database admit 3%; 4
threads admit 8.5%; 8 read threads need 242 retries for 400 reads. Naive
clients see single-digit success under any burst. For "your own Turso" this
is the wall every other scaling discussion hits first.

## Phase 1 — Bounded wait queues (do first)

Replace both `try_acquire_owned` calls (`src/api.rs`: global permit,
per-database slot) with `tokio::time::timeout(ACQUIRE_TIMEOUT,
…acquire_owned())`. Suggested `ACQUIRE_TIMEOUT`: 500 ms, one constant,
documented in `docs/api.md`.

- Preserves the public contract: overload still surfaces as the same `429
  capacity_exceeded`, only after a bounded wait instead of instantly.
- tokio's semaphore is FIFO-fair, so waits don't starve or reorder.
- Slow-client pileup stays bounded by the timeout; memory impact is ~zero
  (waiters hold no connections).

Acceptance: rerun F2b B/B2/C — admission jumps an order of magnitude, p99
stays within ~2× the unloaded write latency, zero non-429 errors. New
regression test: N threads over the permit count all succeed within the
timeout; dropping the timeout to ~0 reproduces today's shedding.

## Phase 2 — Read/write lanes (do second)

Reads must stop losing permits to writes. Two coordinated changes:

1. Split the global semaphore into read and write lanes (suggested starting
   point: 8 read + 4 write permits, justified by re-running phase C at 8–16
   threads until retries vanish, then re-checking memory).
2. Replace the per-database binary semaphore with `tokio::sync::RwLock`
   semantics: read-only requests take `.read()`, writers take `.write()`.
   WAL-mode SQLite supports concurrent readers against one writer; the
   250 ms busy timeout stays as the backstop.

Watch for writer starvation under read floods; if measured, add a
writer-preferring bound (e.g., cap queued readers per database) rather than
reverting to a single slot.

Acceptance: phase C at 8 threads runs retry-free; phase B single-DB write
rate unchanged-or-better; new tests pin concurrent reads during a slow write
plus the starvation bound if one is added.

## Phase 3 — Permit sizing from measurement (do third)

Re-derive every constant from F2b-style runs on the release profile: global
lane sizes, per-database writer count (stays 1 — SQLite), busy timeout,
deadline. Constants stay constants (no config surface) with a comment citing
the benchmark run that set them. This phase is arithmetic + a doc table, not
a redesign.

## Phase 4 — Multi-host placement (later, needs owner targets)

Independent-database placement/movement between hosts stays a later
capability: it needs the still-open host-topology and client-count targets
(F1) plus per-host backup stories (M3). Do not start it on vibe — start it
when one host's measured ceiling (post phases 1–3) is documented and
insufficient for stated targets.

## Non-goals (all phases)

No error-code changes; no unbounded queues; no cross-database atomicity; no
automatic failover; no config surface for constants until two measured
sizing rounds disagree. Client guidance (retry 429 with backoff, prefer
keyed writes) stays valid throughout — phases 1–2 just make it needed
rarely instead of constantly.
