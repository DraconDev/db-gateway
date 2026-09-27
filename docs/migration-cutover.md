# Migration / cutover and rollback plan (Turso → gateway)

**Document role:** Normative migration gates
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)

Plan only — no migration is performed, no Turso system was contacted, and no
application cutover exists yet. The gateway is the migration **target**;
Turso is the source that stays authoritative until stage 5 and is
decommissioned only after the soak. Builds on
`docs/migration-inventory.md` (risks + inventory) and
`tests/migration_fixture.rs` (gateway-side import mechanics, not source
evidence).

## Preconditions (all must hold before stage 1)

1. Inventory complete: every source database has a filled block from
   `docs/migration-inventory.md` (sizes, denied-object census, writers,
   readers, key roles, retry behavior, recovery requirements).
2. All UNVERIFIED markers in the inventory resolved against current Turso
   docs or disposable trial exports (trial import of a cleaned export into a
   scratch gateway data dir, per-statement testing of edge SQL, limit probes
   near our caps, exact-row comparison).
3. Rollback floor in place: a fresh Turso-side export set plus checksums,
   taken within the agreed window and retained untouched through the soak.
4. Gateway targets chosen: fresh names (never tombstoned), credentials
   issued to test identities only, gateway backup coverage running for the
   data dir that will receive the imports.
5. Application owners identified per database; each confirms its read/write
   paths and how it will adopt gateway retry/idempotency behavior (risks
   7/8 in the inventory).

## Stages

### Stage 1 — Pilot selection (reversible)

Pick one source database: smallest writer count, tolerant readers, no
denied SQL objects if possible. Record why it was chosen. Success criterion:
pilot migrates and soaks one full application cycle with zero data
discrepancies.

### Stage 2 — Writer freeze (operator-controlled)

**GATE 1 (irreversible window opens):** owner announces the freeze, stops or
drains each writer on the Turso side, and confirms zero in-flight writes
(Turso-side checks — mechanism UNVERIFIED until the inventory resolves it).
Record the freeze timestamp — it bounds the loss window for everything after.

### Stage 3 — Final export + gateway validation (observable)

1. Export the pilot database from Turso; record size + SHA-256.
2. Validate locally against gateway constraints: file fits the 128 MiB
   lifecycle transfer ceiling (`MAX_TRANSFER_SIZE`). That is the real gate —
   `dg import` always uses the chunked path, which is why the 8 MiB
   single-shot HTTP body limit is not an import ceiling. Above 8 MiB the
   single-shot `/v1/databases/{name}/import` route refuses, but the CLI
   chunks transparently and nothing extra is required of the operator. Also
   no views/triggers/temp objects (risk 3 — redesign if present),
   FK coherence via join spot-checks, statement/limit census from the
   inventory.
3. Transform denied objects into the migration artifact (flattened schema,
   rewritten statements); checksum the artifact. The Turso originals stay
   untouched — Turso remains the source of truth until stage 5.

### Stage 4 — Trial import into the gateway (observable)

Import the artifact into a **scratch gateway data dir** first (never the
production data dir on the first pass). Verify: table/row counts match the
export, indexes present, sample rows byte-identical (including BLOB hex and
NULLs), application read queries return identical results against Turso and
the trial copy. Import into the production data dir under the real target
name only after all checks pass; the trial dir is then discarded (no
tombstone litter in production).

### Stage 5 — Application cutover (operator-controlled)

**GATE 2 (irreversible):** owner switches each application's connection from
Turso to the gateway (credentials replaced per risk 2, retry behavior
re-tuned per risk 7, keyed writes adopted per risk 8). Cut over readers
first, then writers, one application at a time. After each switch, run the
application's own smoke checks plus a Turso-vs-gateway row-count comparison.
**GATE 3 (point of no return):** owner declares the Turso copy frozen only
after every application confirms.

### Stage 6 — Soak and decommission

Keep the Turso exports and rollback floor through the soak period (decided
in gate 8 of `docs/deployment-readiness.md`). Decommission the Turso copy —
and consider any subscription change — only when the soak criterion (stage
1) is met for every database, and only with separate explicit approval:
subscription changes are never implicit in a cutover.

## Rollback conditions

- Any verification mismatch in stage 3/4: stop, keep serving from Turso (it
  was never touched), diagnose, re-stage. No rollback needed — cutover never
  happened.
- Failure after GATE 2 but before GATE 3: switch applications back to Turso,
  reconcile any gateway-side writes made after the cutover timestamp,
  diagnose. Declared data-loss window: gateway-side writes between GATE 2
  and the switch-back.
- Failure after GATE 3: Turso is frozen/stale — rollback means re-freezing
  the gateway side, exporting the delta, and replaying it toward Turso. This
  is the expensive path; it is why GATE 3 requires per-application
  confirmation first.
- The pre-migration Turso export set (precondition 3) is the floor: any
  rollback can always restore service from it, losing at most the writes
  after its timestamp.
