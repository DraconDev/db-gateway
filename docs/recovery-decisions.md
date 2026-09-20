# Recovery decisions (M3 input, not acceptance)

Status: **decision brief, not an accepted policy**. Nothing in this document
is an owner-accepted RPO/RTO/retention guarantee, a backup SLA, or verified
OVH/tool coverage. It compares practical profiles for this single-owner
SQLite service so the owner can choose one. Public sources only; live bucket,
tool-version, and host verification remain open M3 work.

## Decision status (updated 2026-09-19)

- **Selected profile:** none. Profile B remains a provisional recommendation,
  not an authorization to schedule backups or claim an RPO/RTO.
- **RPO/RTO/retention:** owner decision required after cost, resource,
  encryption, and restore evidence are available. The profile numbers below
  are objectives for comparison, not promises.
- **Live boundary:** no OVH account, bucket, credential, backup sidecar, or
  production data may be touched by this brief. Adoption requires the sandbox
  gates in `docs/backup-compatibility.md` §6 and a separate owner approval.
- **Monitoring dependency:** whichever profile is selected must expose the
  newest verified copy, age, failure state, and restore-test result; a cron
  entry without those signals is not backup coverage.

## 1. What is already durable locally (acknowledged)

- Every application SQL request runs in one managed transaction on a fresh
  connection; a write is acknowledged only after commit with
  `journal_mode=WAL` and `PRAGMA synchronous=FULL` (`src/execution.rs`,
  `docs/architecture.md`). A failed statement rolls back the whole request.
- Idempotency replay records commit atomically in the same database
  transaction as the write, so a crash cannot separate them. Key scope is
  (database, principal, key); retention is 24h, at most 256 keys per
  principal per database (`docs/architecture.md`, `docs/api.md`).
- Restart persistence is tested (round-trip write/read across restart), and
  clone/export use the SQLite online backup API for a consistent committed
  snapshot during writes (`docs/api.md`, `TASKS.md`).
- Limits of local durability: **single host, no high availability**. An
  acknowledged commit survives process restart, but host disk loss, host
  failure, or operator deletion after the 24h tombstone purge loses
  everything not copied off-host. Local commit durability does **not** imply
  the latest write has reached any bucket (`docs/architecture.md`).
- The 24h delete tombstone (`DELETE /v1/databases/{name}`, `purge_after`
  retention) is a safety window against accidental recreation, **not** a
  backup. After purge the name can be recreated, but the old file is gone
  unless an export was taken (`docs/api.md`).

## 2. Asynchronous backup exposure (the gap this decision closes)

Any off-host backup copies commits **after** they are acknowledged. Data is
exposed during each of these windows:

1. **Commit-to-upload lag**: writes committed since the last *successful,
   verified* backup copy are local-only. A backup *interval* is an objective,
   not a guaranteed data-loss bound: a failed, partial, or unverified upload
   extends the real exposure window.
2. **Bucket/tool outage**: bucket unavailability, credential/permission
   failure, or backup-process crash stops new recoverable points without
   stopping local writes. Failures must be visible (M3 requirement), not
   silent.
3. **Retention expiry**: a database deleted locally and purged after 24h can
   only be restored from a backup copy retained independently. Tombstone
   retention and backup retention are separate clocks.
4. **Restore dependencies**: a backup file alone is not a running service.
   Recovery needs the gateway binary, the registry (`metadata.sqlite`
   credential hashes and database rows), the per-database file, bucket access
   and manifest/checksum, target disk space, and integrity plus
   application-data verification. `_dg_replay` rows travel inside each
   database file, so a restored database keeps its recent replay history;
   credential scope comes from the registry, which must be recoverable too.

## 3. Profile comparison

“RPO” below means *objective*: the maximum age of the newest verified
recoverable copy under successful operation. Actual loss after a failure can
be larger. “RTO” means time to a verified readable copy on a separate target
under rehearsed conditions, not a production SLA.

| Profile | Backup cadence (objective) | Retention | RPO objective | RTO objective | What it costs | When it fits |
| --- | --- | --- | --- | --- | --- | --- |
| A — manual snapshots only | `export` before risky ops; no schedule | Ad hoc, operator-kept | Unbounded (since last manual export) | Hours (manual import + verify) | ~0; operator discipline only | Pre-M3 development; no production promise |
| B — daily versioned snapshots (recommended start) | 1 verified snapshot per database per day | 7–30 daily versions per database | 24h | ~1h per database to a separate target | Storage = db size × versions retained; one daily upload + verify per db; small compute on the 1 GB host | Single-owner service with daily-change tolerance; simplest restore via existing validated `import` |
| C — frequent snapshots + continuous WAL-style ship | Hourly snapshots or Litestream-style replication | 7d frequent + 30d daily | Minutes–1h | Minutes–1h, tool-assisted | Highest: continuous compute + I/O on 1 GB host, more requests/versions, more monitoring; needs headroom measurement first | Only if the owner rejects 24h exposure after seeing B’s cost/complexity |

Notes:

- Profile A is today’s reality: exports exist and are tested, but nothing
  schedules, verifies, or retains them.
- Profile B maps directly onto already-tested primitives: online-backup
  export, `integrity_check` + foreign-key + protected-table validation on
  import, atomic publication, restore-to-separate-target verification.
- Profile C depends on unverified work: backup-tool selection/version,
  per-database registration and reload behavior, 1 GB headroom, encryption,
  and bucket-outage behavior. It must not be chosen implicitly by shortening
  B’s interval without the M3 verification behind it.

## 4. Recommendation with trade-offs (provisional, not accepted)

**Start with profile B**: one verified snapshot per database per day, 30
daily versions for production databases and 7 for development copies,
restore always to a separate target with integrity and row-level
verification before any cutover.

Why B over A: A’s unbounded RPO is acceptable only while no data matters;
B adds a bounded objective with the smallest new machinery (schedule +
verify + retain around the existing export/import path).

Why B over C: C’s minutes-scale RPO is attractive but unpriced and
unmeasured here — continuous replication competes for the same 1 GB host,
adds failure modes (stalled replication looks healthy until restore), and
requires the tool/bucket verification that is explicitly still open. B’s
daily job can be observed end-to-end (success/failure, age of newest
verified copy) before committing to C.

Trade-offs accepted under B: up to ~24h of committed writes can be lost on
host failure even on a good day, longer on upload/verification failure days;
storage grows with versions retained; restore is an operator-driven import
and verification, not failover — expect ~1h per database, not minutes.

## 5. Cost drivers to watch

- **Retained bytes**: sum over databases of (snapshot size × versions). Small
  SQLite files make B cheap; growth or many dev copies push toward shorter
  dev retention or explicit exclusion lists.
- **Requests and transfer**: daily PUTs plus restore GETs; egress matters on
  restore drills, not steady state.
- **Verification compute**: checksum + `integrity_check` + trial import on
  the host or a scratch target; keep it off the serving path.
- **Operational attention**: backup-health visibility (newest verified copy
  per database, failure alerts) is part of M3 scope; an unverified schedule
  is profile A with a cron entry.

## 6. Unresolved owner decisions (explicit, blocking live work)

None of the following is decided by this brief. Live M3 backup work beyond
local preparation must wait for owner answers:

1. Accept or replace profile B: RPO objective, RTO objective, and retention
   per database class (production vs development).
2. Monthly budget cap for backup storage/requests/egress, and which
   databases are excluded from scheduled backup.
3. Bucket region/endpoint, credential scope, and encryption requirements
   (at-rest and in-transit) for backup objects.
4. Restore-target policy: always a separate database/host, who may authorize
   a restore over an existing name, and how long rollback artifacts are kept.
5. Backup-health contract: where last-successful-backup and staleness are
   exposed (API/CLI), and who is notified on failure.
6. Whether to evaluate continuous replication (profile C) after B is
   verified, and under what measured headroom on the 1 GB host.

## Sources (public, accessed 2026-09-18)

- SQLite, Write-Ahead Logging: https://www.sqlite.org/wal.html
- SQLite, `PRAGMA synchronous`: https://www.sqlite.org/pragma.html#pragma_synchronous
- SQLite, Online Backup: https://www.sqlite.org/backup.html
- SQLite, `PRAGMA integrity_check`: https://www.sqlite.org/pragma.html#pragma_integrity_check
- Litestream documentation (replication concept reference; version/tool selection still open): https://litestream.io/
- OVHcloud Object Storage overview (endpoint/region/S3-compatibility verification still open): https://www.ovhcloud.com/en/public-cloud/object-storage/
- OVHcloud Object Storage S3 API documentation index (credential/permission/retention verification still open): https://help.ovhcloud.com/csm/en-public-cloud-storage-s3/
- Oracle Cloud Always Free resources (micro2 shape/disk/access verification still open): https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm

Local implementation references (not public sources): `src/execution.rs`
(WAL, `synchronous=FULL`, atomic replay), `src/store.rs` (online backup,
tombstone purge), `docs/architecture.md`, `docs/api.md`, `TASKS.md`.
