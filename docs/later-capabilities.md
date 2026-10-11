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
