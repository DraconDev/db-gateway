# Later-capability deferral decisions

**Document role:** Design decision record for the "Later capabilities — not
initial-release blockers" section of `TASKS.md`
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)
Phase 8 (Optional resilience and product layers)
**Status:** Each section below is a **deferred** design decision — the
feature is intentionally not part of the initial release, and the
deferral is recorded here so the box in `TASKS.md` can be checked
with this reference.

The deferrals in this document are not owner-approval items; they
are **deferred by design** as part of the initial-release scope
boundary. Re-evaluation is permitted only after the core path
is proven (Phase 8 conditions in the megaplan: measured capacity,
verified recovery, a rehearsed migration/rollback path).

## 1. Ephemeral databases with TTL, count/memory limits, and explicit crash-loss semantics

**TASKS.md reference:** TASKS.md:197
**Deferral decision date:** 2026-10-11
**Decision:** DEFERRED — not part of the initial release.
**Authority:** Phase 8 of `docs/personal-turso-megaplan.md` (optional
layers are added only after the core path is proven); non-goal
statement in `docs/architecture.md` ("Later independent-database
placement and ephemeral storage are not M0/M1 requirements").

### Why deferred

1. **Initial release stays single-purpose.** The initial product
   promise is "durable, owner-controlled SQLite databases with
   verified recovery." Adding ephemeral storage mixes two durability
   models in one server, complicates the recovery contract, and
   forces the API to expose "is this durable?" semantics on every
   call. The complexity is not justified by any confirmed workload
   on the current 1 GB single-host target.
2. **Capacity ceiling is unmeasured.** A 1 GB host has bounded
   RAM and disk; the count/memory limits of an ephemeral tier
   would compete with the durable tier for the same envelope. The
   resource budget (`docs/resource-budget.md`) is still not ready
   for the durable tier alone, let alone a second tier with its
   own limits.
3. **Crash-loss semantics need a defined threat model.** "Explicit
   crash-loss semantics" requires a clear statement of what is
   lost, when, and how operators see it. That statement is
   missing because no concrete use case is driving it; designing
   it in a vacuum produces either over-broad promises (a new
   RPO class) or over-narrow ones (a non-feature).
4. **Turso migration is the active front.** The current cutover
   plan (`docs/migration-cutover.md`) is blocked on inventory
   UNVERIFIEDs and the soak's preconditions; adding a parallel
   feature path is sequencing debt.

### Design space (when re-evaluated, this is the surface area)

The feature is bounded by three orthogonal axes; a future
implementation will need a concrete answer for each, with tests
named against the same axes. These are **notes for a future
designer**, not commitments; the field can be revised.

#### TTL expiry

- A database is created with an `expires_at` (absolute) or
  `ttl_seconds` (relative) parameter. The owner key is required
  to set either; app keys may only opt in within an
  owner-set maximum.
- Expiry is enforced by a background sweeper that
  tombstone-marks expired databases and runs the same
  24 h purge as durable tombstones. Tombstoning must reject
  a later `dg create` with the same name for the full
  24 h window to avoid races.
- Reaping is observable: a `dg list` shows the expires_at; a
  `dg status` reports the next-sweep time and last-reap
  count.
- **Test axes for re-evaluation:** TTL fires after
  `expires_at`; tombstones prevent reuse for 24 h; an explicit
  `dg create --no-ttl` for a durable database does not
  accidentally pick up a TTL.

#### Count and memory limits

- A registry-level cap on the number of live ephemeral
  databases (`DG_EPHEMERAL_MAX_COUNT`, default unset =
  unlimited) and an aggregate memory cap
  (`DG_EPHEMERAL_MAX_BYTES`, default unset = unlimited).
  Caps are owner-only, set at gateway startup, applied at
  create time. A create that would breach the cap returns
  `capacity_exceeded`; the limit is checked against the
  current live count + estimated file size, not against
  the registry.
- A per-database memory cap is opt-in: a `max_bytes` field
  on create. The gateway writes a `max_bytes` hint into the
  SQLite header (PRAGMA is not allowed for the application,
  so this is metadata, not a runtime enforcement).
- **Test axes for re-evaluation:** the count cap rejects
  the 101st create when set to 100; the memory cap rejects
  when the next create would exceed it; caps are
  per-gateway and survive restart.

#### Crash-loss semantics

- Crash loss is **explicit, not silent**: an ephemeral
  database's data file is *not* in the regular backup set
  (no snapshots, no WAL shipping, no restore path on
  backup side). A crashed-and-restarted gateway loses the
  contents; `dg list` reports the database as
  `tombstoned` and a `dg restore` returns
  `unsupported_for_ephemeral`.
- The API route that creates an ephemeral database MUST
  require a header or flag that the caller understands
  the crash-loss contract; the CLI is the only place
  that header is set without an explicit `--ephemeral`
  flag. A `--ephemeral` create that is then queried with
  `dg backup status` returns `not_backed_up`, not
  `fresh` or `stale`.
- The product-side documentation MUST state, in the
  create-time confirmation, that the data will not survive
  a gateway crash and is not part of the recovery
  contract. A `--yes-i-understand-ephemeral` flag is the
  minimum UX; a confirmation prompt is acceptable.
- **Test axes for re-evaluation:** a `dg backup` command
  on an ephemeral database is rejected or no-ops with a
  clear error; the recovery-contract restore path
  refuses an ephemeral target; a `dg export` of an
  ephemeral database still works (export is not a
  backup — it is a one-shot artifact, useful for the
  caller to keep if they want).

### Acceptance to re-open this decision

A re-evaluation is permitted only when **all** of the
following are true:

- The durable tier has measured capacity on the recorded
  host (gate 5 of `docs/deployment-readiness.md` resolved).
- The recovery contract is verified on a real bucket
  with restore drills passing (gate 7 resolved).
- The Turso cutover is past Stage 6 (soak complete, all
  three sources' GATE 3 crossed) — or the owner
  explicitly cancels the migration.
- A concrete workload is named that the durable tier
  cannot serve and that needs explicit crash-loss
  semantics (not just "a future feature we might
  want").

A re-evaluation that arrives without all four is parked:
this document's deferral stands.

### Cross-references

- `docs/personal-turso-megaplan.md` Phase 8 (optional
  layers, post-core-path only).
- `docs/architecture.md` "Later independent-database
  placement and ephemeral storage are not M0/M1
  requirements" — the non-goal statement.
- `docs/recovery-contract.md` — the recovery contract
  the ephemeral tier must explicitly NOT participate in.
- `docs/observability-contract.md` §2 — the freshness
  signal that must mark ephemeral databases
  `not_backed_up`, not `fresh` / `stale` / `unknown`.
- `TASKS.md:197` — the source row in the "Later
  capabilities" list.

## 2. Optional replay archival and checkpointing where loss of an unfinished match is unacceptable

**TASKS.md reference:** TASKS.md:199
**Deferral decision date:** 2026-10-11
**Decision:** DEFERRED — not part of the initial release.
**Authority:** The first production backup tier is **per-database
consistent verified snapshots**, not continuous WAL-style
shipping — explicitly stated in `docs/architecture.md` ("The
first production bucket profile is per-database consistent
verified snapshots, not a cross-database atomic snapshot"),
`docs/observability-contract.md` ("WAL-style shipping is a
later optional tier and is not the current direction"), and
`docs/personal-turso-megaplan.md` (Profile C continuous WAL
"superseded as the first-tier roadmap direction; retained for
policy history and later evaluation"). The replay-archival /
checkpointing feature is the same shape as Profile C and
inherits the same deferral.

### Why deferred

1. **The first-tier recovery is snapshot-based, not replay-
   based.** A "consistent verified snapshot" (every N minutes
   or every commit, depending on the owner-approved cadence)
   lets a database be restored to a recent point in time; a
   "replay archival" captures every state change so the
   database can be reconstructed from t=0. The current
   recovery contract (`docs/recovery-contract.md`) and
   verified-snapshot path already provide the recovery
   envelope the durable tier needs; an unfinished match
   that was committed in the last snapshot window is
   preserved by the next snapshot, and any work between
   snapshots that did not yet commit is lost.
2. **The cost is real on a 1 GB host.** Continuous WAL-style
   shipping (the closest documented analogue) is rated the
   most expensive of the three evaluated profiles
   (`docs/recovery-decisions.md` § 6.1) — "continuous
   compute + I/O on 1 GB host, more objects/monitoring, plus
   storage/transfer; needs headroom measurement first." The
   resource budget for the durable tier alone is still not
   ready (gate 5 of `docs/deployment-readiness.md`); a
   second tier of I/O on the same envelope is sequencing
   debt, not a feature path.
3. **The match-finalization contract already prevents
   double-reward.** `tests/match_finalization.rs` and
   `tests/replay_deep.rs` prove that a match result
   committed under `Idempotency-Key` is replayed
   byte-identically on retry; an unfinished match is a
   different problem (state in flight, not a finalized
   result), and the unfinished case is not a
   double-reward scenario — it is a "did the final
   INSERT ever land?" question, which the snapshot path
   answers at the next snapshot boundary.
4. **No concrete workload is currently driving the need.**
   The Turso migration (Sources A email-api, B studio-api,
   C billing) has no unfinished-match semantics — each
   application's state is durable per row, and the
   "unfinished match" framing belongs to a competitive or
   real-time system that is not on the migration list.
5. **Turso cutover is the active front.** Sequencing
   discipline: the core path (recovery, migration,
   deployment gates 1-7) is the work; adding a parallel
   replay tier is not.

### Design space (when re-evaluated, this is the surface area)

The feature is bounded by three orthogonal axes; a future
implementation will need a concrete answer for each, with
tests named against the same axes. These are **notes for a
future designer**, not commitments.

#### Replay archival

- A continuous stream of state changes (WAL frames, or a
  higher-level event log) is shipped off-host at a bounded
  cadence. The archival covers the period between verified
  snapshots, so a restore + replay reconstructs the
  database from the last verified snapshot forward.
- Storage: a separate path in the same OVH bucket, or a
  dedicated bucket, decided at adoption time. Retention
  is shorter than the snapshot retention (the snapshot
  tier is the durable floor; the replay tier is the
  short-window recovery overlay).
- Integrity: each shipped frame carries a SHA-256; the
  archival manifest records the start/end LSN (or
  equivalent) of every shipped batch. A replay that
  starts from a snapshot must verify the frame chain
  before applying any state change.
- **Test axes for re-evaluation:** a snapshot + replay
  reconstruct matches the post-snapshot state byte-for-
  byte; a missing or tampered frame is detected before
  any state change is applied; archival retention is
  enforced and older frames are pruned.

#### Checkpointing

- A checkpoint is a per-database, owner-triggered (or
  cadence-triggered) barrier that flushes in-flight state
  to durable storage. The gateway's existing `dg export`
  path (`docs/api.md` Lifecycle: clone, export, import)
  is the building block: a checkpoint is an export to the
  archival location, with a manifest entry recording the
  export as a checkpoint.
- Checkpoint cadence is owner-set (e.g., every 5 minutes
  during live matches, or on a per-match trigger from the
  application). Cadence and trigger live in the gateway
  config or a per-database metadata row, not in the
  archival itself.
- A restart after a crash that lands between two
  checkpoints is documented as "lost the post-checkpoint
  work"; the snapshot is the floor, the checkpoint is the
  higher-resolution floor, and the replay tier is the
  finest-resolution floor.
- **Test axes for re-evaluation:** a checkpoint fires at
  the agreed cadence and the manifest records it; a
  restart after a checkpoint can resume from that
  checkpoint; a restart between checkpoints reports the
  loss window explicitly; a per-match application
  trigger fires a checkpoint reliably.

#### Loss-of-unfinished-match semantics

- The contract is **explicit, not silent**: an
  application that opts in to the replay tier is told
  the loss window (the smaller of: archival retention,
  checkpoint cadence, snapshot cadence). The default
  is "loss of uncommitted in-flight state" — the same
  shape as without the tier, just at a smaller window.
- A "match" that has committed a finalization row
  (the pattern in `tests/match_finalization.rs`) is
  not lost by any tier — the finalization is durable
  the moment the transaction commits; the tier only
  affects state that was in flight.
- The application-side documentation must state the
  loss window per tier. An opt-in API route
  (`Idempotency-Key` already exists; a new
  `Replay-Tier: enabled` header is the minimum UX)
  is the boundary.
- **Test axes for re-evaluation:** an opt-in via
  header is required to enable the tier; the loss
  window is reported in the `dg info` or `dg status`
  output for an opted-in database; an un-opted-in
  database sees no overhead from the tier.

### Acceptance to re-open this decision

A re-evaluation is permitted only when **all** of the
following are true:

- The durable tier has measured capacity on the recorded
  host (gate 5 of `docs/deployment-readiness.md` resolved).
- The recovery contract is verified on a real bucket
  with restore drills passing (gate 7 resolved).
- The Turso cutover is past Stage 6 (soak complete, all
  three sources' GATE 3 crossed) — or the owner
  explicitly cancels the migration.
- A concrete workload is named where loss of an
  **unfinished** match is unacceptable AND the
  existing snapshot path + Idempotency-Key replay
  (proven by `tests/match_finalization.rs` and
  `tests/replay_deep.rs`) is insufficient for that
  workload. "Unfinished" here means state in flight
  that has not committed a finalization row, not
  "the match could be replayed from t=0" in
  general.

A re-evaluation that arrives without all four is
parked: this document's deferral stands.

### Cross-references

- `docs/architecture.md` M1 design constraints
  (per-database consistent verified snapshots is the
  first-tier profile).
- `docs/observability-contract.md` §1 (backup
  freshness signal) and §2 (first-tier is snapshots;
  WAL-style is a later optional tier, not the
  current direction).
- `docs/personal-turso-megaplan.md` source-of-truth
  register: Profile C "superseded as the first-tier
  roadmap direction; retained for policy history and
  later evaluation".
- `docs/recovery-decisions.md` §6.1 (Profile C
  research brief with cost analysis).
- `docs/recovery-contract.md` (the recovery contract
  the replay tier must integrate with, not bypass).
- `tests/match_finalization.rs` and
  `tests/replay_deep.rs` — the existing contracts
  the replay tier must not weaken.
- `TASKS.md:199` — the source row in the "Later
  capabilities" list.
