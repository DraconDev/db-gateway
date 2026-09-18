# Migration / cutover and rollback plan

Plan only — no migration is performed, no target exists yet, and no Turso or
application system was contacted. Builds on `docs/migration-inventory.md`
(risks + inventory) and `tests/migration_fixture.rs` (gateway-side import
mechanics, not target evidence).

## Preconditions (all must hold before stage 1)

1. Inventory complete: every database has a filled block from
   `docs/migration-inventory.md` (sizes, writers, readers, key roles,
   idempotency callers, recovery requirements).
2. All UNVERIFIED markers in the inventory resolved against current target
   docs or a disposable trial (trial import of a `_dg_*`-stripped export,
   per-statement testing of edge SQL, limit probes near our caps).
3. Gateway backups current: a fresh versioned export set plus checksums
   (`docs/recovery-contract.md`) taken within the agreed window.
4. Target group/plan chosen with headroom for the largest database plus
   growth; auth tokens issued to a test identity only.
5. Application owners identified per database; each confirms its read/write
   paths and its retry/idempotency behavior (risk 7/8 in the inventory).

## Stages

### Stage 1 — Pilot selection (reversible)

Pick one database: smallest writer count, tolerant readers, no idempotency
callers if possible. Record why it was chosen. Success criterion: pilot
migrates and soaks one full application cycle with zero data discrepancies.

### Stage 2 — Writer freeze (operator-controlled)

**GATE 1 (irreversible window opens):** owner announces the freeze, stops or
drains each writer, and confirms zero in-flight writes (gateway shows no new
`_dg_replay` rows and row counts stable across two consecutive checks).
Record the freeze timestamp — it bounds the loss window for everything after.

### Stage 3 — Final export validation (observable)

1. `dg export` the pilot database; record size + SHA-256.
2. Validate locally: row counts per table, FK coherence via join spot-checks,
   `_dg_*` tables listed for stripping.
3. Strip server-owned tables (`_dg_replay` at minimum) into the
   migration artifact; checksum the artifact. The gateway originals stay
   untouched — the gateway remains the source of truth until stage 5.

### Stage 4 — Separate-target import (observable)

Import the artifact into a **separate** target database (never the planned
production target on the first pass — a disposable trial target). Verify:
table/row counts match export, indexes present, sample rows byte-identical
(including BLOB hex and NULLs), application read queries return identical
results against old and new. Promote the trial target to production target
only after all checks pass.

### Stage 5 — Application cutover (operator-controlled)

**GATE 2 (irreversible):** owner switches each application's connection from
the gateway to the target (credentials replaced per risk 2, retry behavior
re-tuned per risk 7, idempotency redesigned per risk 8). Cut over readers
first, then writers, one application at a time. After each switch, run the
application's own smoke checks plus a gateway-vs-target row-count comparison.
**GATE 3 (point of no return):** owner declares the gateway copy read-only
(or tombstoned per lifecycle rules) only after every application confirms.

### Stage 6 — Soak and decommission

Keep gateway exports running through the soak period (decided in gate 8 of
`docs/deployment-readiness.md`). Decommission the gateway copy only when the
soak criterion (stage 1) is met for every database.

## Rollback conditions

- Any verification mismatch in stage 3/4: stop, keep serving from the
  gateway (it was never touched), diagnose, re-stage. No rollback needed —
  cutover never happened.
- Failure after GATE 2 but before GATE 3: freeze target writers, re-export
  any target-side writes made after the freeze timestamp, reconcile against
  the gateway copy, switch applications back. Declared data-loss window:
  target-side writes between GATE 2 and the freeze-back.
- Failure after GATE 3: gateway copy is read-only/stale — rollback means
  re-freezing the target, exporting the delta, and replaying it through the
  gateway import path. This is the expensive path; it is why GATE 3 requires
  per-application confirmation first.
- The gateway pre-migration export set (precondition 3) is the floor: any
  rollback can always restore service from it, losing at most the writes
  after its timestamp.
