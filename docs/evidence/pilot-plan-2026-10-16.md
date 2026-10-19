# Pilot plan — one real non-critical workload (2026-10-16)

Status: **plan + rollback rehearsal done; pilot launch awaits owner
decision** (workload choice + trial-import approval), then a 7-day soak
clock the agent cannot fast-forward.

## Rollback rehearsal (done, transcript kept)

`scripts/pilot-rollback-rehearsal.sh` → transcript
`docs/evidence/pilot-rollback-rehearsal-2026-10-16.log` (exit 0): app-like schema
(users + usage with FK, mixed types), rollback export, continued writes,
second export, validated import into a fresh target (host-replacement
rollback, contract Path B). Rolled-back copy: 4/4 rows exact across both
batches (units sum 25), `integrity_check ok`, FK violations 0, source
unchanged, copy independently writable. This is the procedure the pilot
will use if rollback is ever needed: `dg export` on micro2, copy down,
`dg import` into a fresh local target, verify counts + integrity.

## Workload candidates (from `docs/migration-inventory.md`)

| Candidate | Shape | Why it fits / does not fit |
|---|---|---|
| **C-dev: `dracon-billing-dev` (RECOMMENDED)** | 12 tables, dev database, one-shot migrate script (no ongoing writer) | Dev, non-critical, real schema; no live-writer freeze problem. Blockers: remote size/rows UNVERIFIED; needs owner trial-import approval per cutover gate. |
| A: email-api queue | 1 table (`email_queue`), queue semantics (replayable) | Small, but production-adjacent (sends real mail on replay risk); needs 3× ALTER flattening + `Idempotency-Key` adoption first (risk 8). More prep than C-dev. |
| B: studio-api | 22 tables, TWO ongoing writers (handlers + poller) | Largest blast radius; 14× ALTER flattening; freeze requires stopping two writers. Worst first pilot. |

Recommendation rationale: C-dev is the only candidate with no ongoing
production writer and dev blast radius. The ALTER/DROP-COLUMN migration
code (state.rs:847,866,932) must still be flattened to gateway-compatible
DDL at trial import — that work is inside the pilot, not before it.

## Soak plan (starts at pilot launch)

- Pilot DB on micro2, accessed **only** via database-scoped key over
  `ssh -L 7432:127.0.0.1:7432` tunnel (loopback-only serving unchanged).
- Green criteria for 7 consecutive days: hourly `dg backup verify ok`
  sets (age < 26 h, `attempts.jsonl` outcome ok) **and** `litestream
  status ok` with advancing txid.
- Daily check: one read-only probe (counts + freshness), recorded.
- Rollback trigger: any red day, or owner call. Rollback = the rehearsed
  export/import above; Turso stays authoritative until the pilot's own
  GATE 2 (never implicit).

## Open owner decisions (the pause below carries these)

1. Workload: C-dev as recommended, or A, or B?
2. Approve creating the pilot database + scoped key on micro2 and
   starting the 7-day clock?
3. After 7 green days: promote, extend, or roll back?
