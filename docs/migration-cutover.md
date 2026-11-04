# Migration / cutover and rollback plan (Turso → gateway)

**Document role:** Normative migration gates
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)

Plan only — no migration is performed, no Turso system was contacted, and no
application cutover exists yet. The gateway is the migration **target**;
Turso is the source that stays authoritative until stage 5 and is
decommissioned only after the verification. Builds on
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
   taken within the agreed window and retained untouched through the verification.
4. Gateway targets chosen: fresh names (never tombstoned), credentials
   issued to test identities only, gateway backup coverage running for the
   data dir that will receive the imports.
5. Application owners identified per database; each confirms its read/write
   paths and how it will adopt gateway retry/idempotency behavior (risks
   7/8 in the inventory).

## Per-production cutover plan

The generic Stages 1–6 below are run **once per production source**, in
sequence (Source A → Source B → Source C), with each cutover completing
its full lifecycle — including the verification — before the next begins. The
sources named in `docs/migration-inventory.md` § Source survey
(2026-10-11) are:

| # | Source | Writers to freeze | Gateway target | Status |
|---|---|---|---|---|
| A | `email-api` database (`TURSO_EMAIL_URL` / `TURSO_EMAIL_TOKEN`) | `email-api` handlers via `api_core::db::SharedDb` + `ConnectionSupervisor` (state.rs:166) | (unassigned) | inventory complete; remote sizes/rows UNVERIFIED |
| B | `studio-api` database (`STUDIO_API_DB_URL` / `STUDIO_API_DB_TOKEN`) | `studio-api` handlers via SharedDb + supervisor (state.rs:145) **and** a dedicated poller with its own `poller_database` / `poller_conn` (state.rs:680) | (unassigned) | inventory complete; remote sizes/rows UNVERIFIED |
| C | `billing` databases (`example-billing`, `example-billing-dev`) | `example-application` handlers via SharedDb + supervisor (state.rs:592); one-shot `migrate-turso-omega-cleanup.py` is not an ongoing writer | (unassigned) | inventory complete; remote sizes/rows UNVERIFIED |

The pilot source from Stage 1 is selected from these three. Each
subsequent production cutover repeats Stages 2–6 with the named source's
freeze mechanism and application set; a verification failure or rollback in one
source blocks the next.

## Per-source freeze mechanism (avoiding accidental dual writers)

The dual-writer risk is concrete: every production source uses
`api_core::db::ConnectionSupervisor` (a tokio task that probes the lane
every 5 s and rebuilds a dead Hrana stream against the same Turso URL —
see `apis/libs/api-core/src/db.rs:1214-1255`). While the application
process is up, the supervisor will keep the Turso connection warm and
reconnect after any failure, so a "freeze" that does not stop the
process is not a freeze. There is no per-writer kill switch and no
in-process drain — the only safe mechanism is **process-level
freeze + env-var swap before restart**, repeated per source:

### Source A — email-api

1. **Freeze:** stop the `email-api` service (`systemctl stop email-api`
   or equivalent). Confirm: zero `email-api` processes, zero open
   Hrana streams to `TURSO_EMAIL_URL` from this host (Turso-side check
   — mechanism UNVERIFIED until the inventory UNVERIFIEDs resolve).
2. **Cutover gateway-side** per Stages 3–5 below; credentials in
   `/etc/email-api/dg.env` (or its current home) are rewritten to
   point at the gateway, the original Turso env vars are retained
   untouched on a rollback path.
3. **Dual-writer avoidance:** the supervisor cannot resume a Turso
   connection because the process is stopped; the new env vars cause a
   gateway-side connection on next start. No envelope-window where
   both targets are reachable from a running writer.
4. **Confirmation before GATE 2:** `ps` shows no `email-api`;
   `ss -tn` from this host to `TURSO_EMAIL_URL` port returns zero
   established connections attributable to `email-api`.

### Source B — studio-api (two writers)

1. **Freeze:** stop the `studio-api` service. This kills BOTH the
   handler SharedDb supervisor (state.rs:145) AND the dedicated poller
   (state.rs:680) — they are independent connection pools and the
   poller does not share the supervisor's lane, so "stopping the
   handlers" alone is not sufficient.
2. **Confirm:** zero `studio-api` processes, zero established Hrana
   streams to `STUDIO_API_DB_URL` from this host, no poller
   `tokio::task` running. Turso-side check mechanism UNVERIFIED.
3. **Cutover gateway-side** per Stages 3–5 below; credentials swap
   identically.
4. **Dual-writer avoidance:** process-level freeze removes both
   writers; gateway env vars point a single SharedDb + a single
   replacement poller at the gateway on next start.
5. **Confirmation before GATE 2:** same as Source A plus a
   `ps -L -p $(pgrep studio-api)` (must be empty) and a 60 s wait to
   confirm the poller is not respawned by a scheduler entry.

### Source C — example-application

1. **Freeze:** stop the `example-application` service. The one-shot
   `migrate-turso-omega-cleanup.py` script is not an ongoing writer
   and does not need a freeze of its own — confirm it is not
   currently running (`pgrep -f migrate-turso-omega-cleanup.py`
   empty) before proceeding.
2. **Confirm:** zero `example-application` processes, zero established Hrana
   streams to `TURSO_BILLING_URL` from this host, no `migrate-` script
   running. Turso-side check mechanism UNVERIFIED.
3. **Cutover gateway-side** per Stages 3–5 below.
4. **Dual-writer avoidance:** identical — process-level freeze and
   env-var swap.
5. **Confirmation before GATE 2:** same shape.

The pattern is uniform on purpose: **stop → confirm zero writers →
swap env vars → restart only after GATE 2**. A partial cutover
(swap env vars without stopping) is the dual-writer scenario this
section is here to prevent.

## Stages

### Stage 1 — Pilot selection (reversible)

Pick one source database: smallest writer count, tolerant readers, no
denied SQL objects if possible. Record why it was chosen. Success criterion:
pilot migrates and verifications one full application cycle with zero data
discrepancies.

### Stage 2 — Writer freeze (operator-controlled)

**GATE 1 (irreversible window opens):** owner announces the freeze, stops or
drains each writer on the Turso side, and confirms zero in-flight writes
(Turso-side checks — mechanism UNVERIFIED until the inventory resolves it).
Record the freeze timestamp — it bounds the loss window for everything after.

### Stage 3 — Final export + gateway validation (observable)

1. Export the example database from Turso; record size + SHA-256.
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

### Stage 6 — verification and decommission

Keep the Turso exports and rollback floor through the verification period (decided
in gate 8 of `docs/deployment-readiness.md`). Decommission the Turso copy —
and consider any subscription change — only when the verification criterion (stage
1) is met for every database, and only with separate explicit approval:
subscription changes are never implicit in a cutover.

## Post-cutover verification (verification-period checks)

The four areas named in TASKS.md:190 — schemas, row counts/data,
application behaviour, backup coverage — must all pass at every checkpoint
below, per source, before that source is considered cutover-complete. The
verification duration itself is owner-decision-bound (gate 8 of
`docs/deployment-readiness.md`); the checks below define what "passing the
verification" means, not how long the verification runs.

**Checkpoints.** Three repeatable checkpoints, per source:

- **C1 — At verification start** (immediately after GATE 3 for the source):
  baseline the four areas against the Turso export set retained on the
  rollback floor.
- **C2 — periodic during verification**: drift checks; the same comparisons re-run
  to catch silent regressions, schema drift, or backup-side issues
  before the verification ends.
- **C3 — At verification end** (before GATE 3 → decommission): the final
  pass-or-fail record that the verification criterion was met for the source.

A failed check at any checkpoint pauses that source's verification: the cutover
is not considered complete, GATE 3 is not crossed, and the rollback
conditions below apply. The Turso copy stays authoritative through the
verification.

### A. Schema verification (per source, per checkpoint)

Run against the gateway database, comparing to the migration artifact
from Stage 3:

- DDL matches: per-table `CREATE TABLE` (columns, types, defaults, NOT
  NULL, CHECK, COLLATE) is byte-identical to the migration artifact;
  index definitions match the inventory's pre-cutover list.
- Foreign keys present: every FK from the inventory's pre-cutover list
  exists on the gateway side; `SELECT count(*) FROM
  pragma_foreign_key_check` returns `0`.
- No unexpected `_dg_*` tables: only the canonical `_dg_replay` (when
  used) and `_dg_*` protection tables; any other `_dg_*` match is an
  incident.
- `PRAGMA integrity_check` returns exactly `ok` (re-used from
  `docs/recovery-contract.md` §4).

Pass criterion: every line above matches the migration artifact;
integrity_check is `ok`.

### B. Row counts and data verification (per source, per checkpoint)

Turso is still live during the verification; comparisons run against the
rollback-floor export set (C1) and against an on-demand re-export (C2,
C3) so the comparison target does not move under the gateway.

- Per-table row counts: gateway `SELECT count(*) FROM <t>` matches the
  Turso export set for every table in the inventory.
- Spot-check queries: a curated set of representative SELECTs (covering
  joins, aggregates, ordering, NULL handling, BLOB columns) returns
  byte-identical results, including BLOB hex encoding and NULL vs empty
  string — counts alone are insufficient (re-used from
  `docs/recovery-contract.md` §4 application-data checks).
- Drift threshold: C2 periodic re-runs accept zero row-count drift; a
  one-row drift that maps to a documented, expected application write
  is acceptable only when reconciled in the verification log.
- Sample of recently-written rows: a writer path known to be live (per
  the inventory's per-source writer list) produces rows that match
  application expectations on the gateway.

Pass criterion: all counts and spot-checks match; drift reconciled or
zero; sample rows match application expectations.

### C. Application behaviour verification (per source, per checkpoint)

The application set is the per-source set named in the Per-production
cutover plan above (email-api, studio-api, example-application):

- Application smoke checks pass: the per-app smoke set recorded in the
  inventory's per-source notes returns 200/expected for every request
  in the set; the per-app comparison to the pre-cutover Turso
  baseline (decision 8) shows no regressions.
- Retry behaviour: 429/408 responses are handled per the gateway
  contract (`docs/api.md`); no unbounded client-side retry loops
  appear in the application logs.
- Idempotency-Key adoption: writers required to use
  `Idempotency-Key` (per risk 8 in the inventory) send it on every
  qualifying write; replay under the same key returns the same result
  (covered by `tests/replay_deep.rs`).
- Error profile: zero 5xx responses attributable to the gateway on the
  request path during the verification; 4xx responses are within the
  documented envelope.
- Latency budget: p50/p99 latency stays within the agreed budget
  against the Turso baseline from the inventory; the budget is the
  owner's, not implicit.

Pass criterion: smoke checks pass; retry/idempotency behaviour matches
the contract; zero unexpected 5xx; latency within budget.

### D. Backup coverage (per source, before C1 closes, re-checked at C2 and C3)

Backup coverage is a precondition for the next production cutover, so
it must be live by C1 of the current source's verification — not at the end of
all verifications.

- Backup set inclusion: `dg backup status` lists the new gateway
  database; the backup root's manifest contains the source name (per
  the inventory); no `_dg_replay` shadow matches.
- First verified snapshot: a verified snapshot exists within the
  agreed cadence (decision 8) — verified meaning checksum + integrity
  + FK + protected-table + application-data checks all pass on a
  separate target, as defined by `docs/recovery-contract.md` §4.
- Restore drill: one restore drill is run on the first verified
  snapshot of the source, into a separate target (registry-restored
  path or fresh-registry path), and the §4 checks all pass.
- Freshness signal: the observability contract's backup-freshness
  signal (`docs/observability-contract.md` §2) reports `fresh` for
  the source, not `stale` or `unknown`; a failed-attempt notice fires
  on the first failed attempt (§3a).
- Recovery rehearsal equivalence: the source's per-cutover backup
  behaviour matches the recovery contract's `Pass` criteria.

Pass criterion: set inclusion confirmed; first verified snapshot
exists; one restore drill passes §4; freshness signal is `fresh`;
alert delivery confirmed (one test failure-and-notice during the
verification, or explicit dry-run).

### E. verification exit (per source)

A source's verification is complete when every check in A–D has passed at C1,
C2 (every day), and C3 for that source. The source's GATE 3 is
crossed only after this record exists; the next source's cutover
(per the per-production plan) starts only after the prior source's
GATE 3.

The full cutover lifecycle is complete when every source's GATE 3
has been crossed; Turso decommission and any subscription change
follow the Stage 6 conditions above and remain a separate explicit
owner approval.

## Pre-cancellation gate (before any Turso cancellation / downgrade)

A Turso **cancellation or downgrade** is a separate, terminal action
that lives one level above the cutover gates above. It is **never**
implicit in a successful cutover or verification, and it is **never** part of
the Stage 6 decommission. The two halves — Turso source decommission
(deleting the database copy) and subscription cancellation (ending
the platform relationship) — are both gated by the same pre-cancellation
checks below, plus separate explicit owner approval. Crossing this gate
removes the rollback floor; everything after is a deliberate
single-direction move with no Turso copy to fall back to.

The pre-cancellation gate has three blocks, all of which must pass
before any cancellation/downgrade request is opened:

### 1. Rollback-artifact retention rule

The Turso-side export set taken in precondition 3 is the rollback
floor. Before any cancellation/downgrade:

- **Location and naming:** the export set is held in the agreed
  off-host location (the Turso-side export path) with the agreed
  naming convention (one directory per source: A, B, C) and a
  recorded `manifest.json` listing every file with its size and
  SHA-256.
- **Retention period:** the export set is retained for at least
  the verification period (gate 8 of `docs/deployment-readiness.md`) **plus
  one additional owner-approved window** (a separate owner decision;
  the verification-period length and the post-verification retention window are not
  the same thing — the verification proves correctness, the retention
  window is the operator's own insurance against late findings).
  Early deletion requires an explicit owner decision recorded in
  the verification log; the default is "retain through the post-verification
  window, then re-decide."
- **Integrity:** every export file's SHA-256 matches the
  `manifest.json`; one restore drill on a randomly chosen export
  per source returns the §4 Pass result (`docs/recovery-contract.md`).
- **Coverage:** the export set covers every source named in the
  per-production cutover plan (A, B, C); no source is cancelled
  without its export present and verified.

A cancellation request without a complete, integrity-checked,
coverage-complete export set is **refused at the gate**, not
deferred.

### 2. Client-dependency confirmation (no live clients on Turso)

Before any cancellation/downgrade, confirm that **no client still
depends on the Turso copy** for any of the three sources. This is
not "the cutover is done" — it is a concrete, observable state
across the application set, the host, and the Turso side.

- **Per-application env config:** every application's env file /
  secret store no longer references the Turso URL
  (`TURSO_EMAIL_URL`, `STUDIO_API_DB_URL`, `TURSO_BILLING_URL`,
  and the matching token vars) — they reference the gateway
  credentials only. Verified by reading each application's
  authoritative env file on the live host and grepping for the
  Turso URL strings; the result is recorded in the verification log.
- **Per-application runtime check:** the running process for each
  application (email-api, studio-api, example-application) has open
  connections only to the gateway, not to Turso. Verified by
  `ss -tnp` (or equivalent) from each host: zero established
  connections to the Turso URL/port attributable to the
  application process. A 60 s wait and a second read is
  recommended to catch slow pollers (Source B's poller is the
  one to watch for).
- **Turso-side access log:** the Turso platform's access log for
  each source database shows **zero reads or writes** from any
  client since GATE 3 of the source's cutover. This is the
  definitive proof from the source side. Mechanism (UI vs API)
  is the operator's; the *criterion* is the access log: zero
  entries after the source's GATE 3 timestamp, with the
  monitoring window at least the verification length.
- **Edge crawlers / dashboards / scheduler jobs:** every external
  reader (a Turso studio/CLI session, a scheduler-driven script, a
  dashboard query) is inventoried in the migration-inventory
  and explicitly retired or re-pointed at the gateway before
  the cancellation request is opened. The inventory's
  per-source writer list is the canonical reference; the
  retirement record is the proof.

Pass criterion: all four checks above pass. Any one failing —
including a single Turso access log entry after the source's
GATE 3 — is a hard hold: the cutover is not considered
client-clean, and cancellation is refused.

### 3. Separately authorized owner approval

The cancellation/downgrade request is a **separate owner decision**,
not a step in any cutover's lifecycle, and not a stage in this
plan. The request must include, in writing:

- The identity of the cancellation action (subscription tier
  change, database deletion, group deletion, full platform
  closure — whichever applies).
- The retention-window decision (default: retain through the
  post-verification window; non-default: an explicit shorter window
  with the owner-signed reason).
- The pass record for blocks 1 and 2 (export set integrity +
  client-dependency confirmation) with dates and operator
  signatures.
- The list of source databases (A, B, C) the cancellation
  applies to; partial cancellations (e.g. "downgrade but keep
  the dev billing database") are allowed only when the
  per-source coverage of blocks 1 and 2 is re-checked for
  every kept source.

A cancellation request that arrives without a written owner
decision covering these four points is **not executable**;
the operator escalates, does not improvise.

### Refusal conditions (gate fails)

The pre-cancellation gate fails — and the cancellation request
is refused — if any of the following holds:

- Any export file is missing, fails its SHA-256 check, or has
  no recorded `manifest.json` entry.
- A restore drill on any source's export returns anything
  other than the recovery-contract §4 Pass result.
- The per-application env config still references the Turso
  URL for any source.
- `ss -tnp` shows any established connection to the Turso
  URL/port from the application host.
- The Turso access log shows any read or write entry after the
  source's GATE 3 timestamp inside the monitoring window.
- An edge crawler, dashboard, or scheduler-driven reader is
  unaccounted for in the per-source retirement record.
- The owner decision is missing any of the four points above.

In every refusal case, the Turso copy stays untouched and
the cancellation is held until the gate is met; rolling the
export set forward to a fresh capture resets only the
integrity check, not blocks 2 and 3.

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
