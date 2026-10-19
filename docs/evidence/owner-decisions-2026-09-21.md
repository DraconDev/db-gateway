# Owner decision memos (proposed 2026-09-21, NOT accepted)

Each section below is a **recommendation** for one open owner decision from
`docs/recovery-decisions.md` §6, `docs/deployment-readiness.md` gate 8, and
the audit tasklist. Nothing here is accepted policy and nothing authorizes
live action — the owner approves, edits, or rejects each item. Fields marked
`[FILL]` need an owner-supplied value no memo can invent.

How to accept: reply with per-item `accept` / `accept with edit: …` /
`reject`, or edit this file and say so. Accepted items move into
`docs/recovery-decisions.md` / `docs/deployment-readiness.md` as policy.

## R1. Profile C objectives (recovery-decisions §6.1)

**Recommend: ACCEPT** the working objectives as planning targets with the
existing "objectives, not SLA" qualifier: 5-minute lag/RPO objective under
successful verified operation, 15-minute local RTO objective, retention 7
days frequent + 30 daily production / 7 daily development versions, every-sync
verification, daily separate-target restore drill.

Rationale: matches the stated preference for eager recovery over a 24-hour
exposure window; the fallback (manual validated snapshots) stays defined for
any failed adoption gate; the qualifier prevents the objectives from being
read as guarantees before evidence exists.

Owner action: accept, or replace any number above.

## R2. Backup budget + exclusions (recovery-decisions §6.2)

**Recommend: formula now, number after the sandbox.** Set the monthly cap
only once the sandbox measures real bytes (retained bytes = Σ db size ×
versions + WAL/object overhead + restore egress). Interim rule: back up all
databases except an explicit exclusion list kept in the backup runbook
(suggested initial entries: `dev_*`, `scratch_*`, throwaway drill names).
Default is include-all — exclusion must be a deliberate entry, never a
missed registration.

Owner action: `[FILL]` monthly cap in EUR after sandbox bytes are known;
confirm or edit the exclusion default.

## R3. Bucket region, credentials, encryption (recovery-decisions §6.3)

**Recommend:**
- Region: the OVH region nearest micro2's region (same continent at
  minimum) to minimize steady-state latency and egress on restore drills.
  Exact region chosen at provisioning time from current OVH docs.
- Credentials: a dedicated backup-only user + keypair, least-privilege scope
  to the backup bucket(s) only, no console/API access beyond object I/O.
  Keys live in owner secret management, never in this repo or the unit file.
- Encryption: SSE-OMK (OVH-managed, transparent) over SSE-C. SSE-C's
  per-request key ownership turns any key loss into unrecoverable backups —
  an operator footgun with no upside at this scale. TLS in transit is
  non-negotiable (S3 SigV4 over HTTPS, the documented default).

Owner action: `[FILL]` exact region at provisioning; confirm SSE-OMK or
justify SSE-C with a key-custody plan.

## R4. Restore-target policy (recovery-decisions §6.4)

**Recommend:**
- Restores always land on a **separate** database name or host first;
  verification (checksum, integrity, FK, protected-table, exact rows) passes
  there before anything else moves.
- Overwriting an existing live name additionally requires owner typed-name
  confirmation at restore time (same UX as `dg delete --confirm NAME`).
- Rollback artifacts (pre-restore exports) are kept 30 days, minimum 2
  versions per database, then pruned.

Owner action: accept, or adjust retention numbers.

## R5. Backup-health contract (recovery-decisions §6.5)

**Recommend:** implement the freshness signals from
`docs/observability-contract.md` §2 as a `dg backup status` CLI command plus
a matching API route (tasklist C2): per database, last attempt/success/
failure, newest verified copy + age, failure reason, registry inclusion, and
last restore-test result. Alert threshold: committed changes unrepresented
by a verified copy for >300 s. Notification channel: status command + logs
first; push alerting (email/webhook) is a later explicit choice, not part of
initial backup scope.

Owner action: `[FILL]` if a push channel is required at launch, and which.

## R6. Sandbox acceptance bar (recovery-decisions §6.6)

**Recommend:** Profile C is adopted only when a 7-day sandbox run shows ALL
of: p99 commit-to-verified-copy lag ≤ 5 min; 3 consecutive separate-target
restores each ≤ 15 min with exact rows; sidecar RSS ≤ 256 MiB steady-state;
zero unproven cells in the §6 operation/restore/retention/failure matrices;
registry (`metadata.sqlite`) restore proven with scoped keys resolving. Any
miss blocks rollout and keeps manual snapshots as the fallback.

Owner action: accept, or adjust numbers/windows.

## R7. Upgrade soak + snapshot retention (gate 8, part 1)

**Recommend:** keep the previous binary plus both pre-upgrade snapshot legs
for 7 days after an upgrade; delete only after the owner confirms the new
version healthy. Retain the 2 most recent pre-upgrade snapshots per database
thereafter.

Owner action: accept, or adjust days/count.

## R8. Owner credential custody (gate 8, part 2)

**Recommend:** the owner holds the only durable copy of each owner key
(micro2's `/etc/dg/owner.key` exists on-host for service operation), stored
in the owner's existing secret management (age-encrypted, consistent with
the dracon-warden-managed files in this workspace) — never in chat logs,
repos, or unencrypted notes. No second custodian until the owner names one.

Owner action: `[FILL]` confirm storage location; name a second custodian or
explicitly defer.

## R9. Post-snapshot write-loss window (gate 8, part 3)

**Recommend: ACCEPT** that a rollback loses every write made after its
snapshot, mitigated by a mandatory pre-rollback export of anything worth
keeping (already in `docs/upgrade-rollback.md`). No design change; this is
the documented cost of snapshot rollback.

Owner action: accept, or require a different rollback mechanism.

## R10. MemoryMax method (resource budget)

**Recommend:** set `MemoryMax` from D1 measurement, not sooner, by formula:
measured sustained peak RSS × 1.5, rounded up to 64 MiB, capped so ≥256 MiB
stays for OS + page cache on the 1 GiB host. The line stays commented until
D1 numbers exist.

Owner action: accept the method; the value follows measurement.

## R11. Log retention (deployment)

**Recommend:** rely on the host journal for the JSON stderr stream with an
explicit cap set at install time (suggested: 500 MiB / 30 days via
`journald.conf`), recorded in the install runbook. No gateway-side log files.

Owner action: accept, or set different caps.

## R12. Turso source survey (migration precondition)

**Recommend:** approve a **read-only** Turso source survey (list databases,
sizes, schemas, writers) as the next live step — but only after gateway
backup coverage exists for the data dir that will receive imports. No
exports, no writes, no subscription changes in that step; each later live
step needs its own approval.

Owner action: approve the survey step (with window), or defer.
