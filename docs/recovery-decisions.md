# Recovery decisions (M3 input, not acceptance)

**Document role:** Historical/current research brief; the canonical roadmap is
[`personal-turso-megaplan.md`](personal-turso-megaplan.md). For the first
production tier, the roadmap selects **per-database consistent verified
snapshots**; the Profile C continuous-WAL proposal below remains a research
option for a later measured tier, not the current first-tier direction.

Status: **decision brief and working local policy, not an accepted production
policy**. The owner has selected Profile C objectives for local preparation,
but nothing in this document is an owner-accepted RPO/RTO/retention guarantee,
a backup SLA, or verified OVH/tool coverage. Public sources only; live bucket,
tool-version, and host verification remain open M3 work.

## Decision status (updated 2026-09-20)

- **Working profile:** C — continuous WAL-style shipping with a five-minute
  lag objective after committed changes, seven days of frequent points, and
  daily history of 30 production / 7 development versions.
- **Working restore objective:** verify every sync and rehearse a separate-
  target restore daily; target a 15-minute local RTO objective, with owner
  authorization required before any cutover.
- **Policy status:** these are owner-selected objectives for local preparation,
  not an accepted production RPO/RTO/retention guarantee. The local rehearsal
  reports a measurement but does not prove sidecar timing or host fit.
- **Failure fallback:** an unresolved adoption gate blocks rollout; manual
  validated snapshots remain the fallback.
- **Live boundary:** no OVH account, bucket, credential, backup sidecar,
  production data, or further micro2 action may be touched by this brief.
  Adoption requires the sandbox gates in `docs/backup-compatibility.md` §6
  and a separate owner approval.
- **Monitoring dependency:** the implementation must expose the newest
  verified copy, lag/age, failure state, and restore-test result; a scheduler
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
| B — daily versioned snapshots | 1 verified snapshot per database per day | 7–30 daily versions per database | 24h | ~1h per database to a separate target | Storage = db size × versions retained; one daily upload + verify per db; small compute on the 1 GB host | Single-owner service with daily-change tolerance; simplest restore via existing validated `import` |
| C — frequent snapshots + continuous WAL-style ship | Continuous WAL-style shipping; five-minute lag objective after committed changes | 7d frequent + 30 daily production / 7 daily development | 5m objective under successful verified operation; outage can extend it | 15m local rehearsal objective; not a production SLA | Highest: continuous compute + I/O on 1 GB host, more objects/monitoring, plus storage/transfer; needs headroom measurement first | Owner-selected working profile for local preparation; adoption remains gated |

Notes:

- Profile A is today’s reality: exports exist and are tested, but nothing
  schedules, verifies, or retains them.
- Profile B maps directly onto already-tested primitives: online-backup
  export, `integrity_check` + foreign-key + protected-table validation on
  import, atomic publication, restore-to-separate-target verification.
- Profile C still depends on unverified work: backup-tool selection/version,
  per-database registration and reload behavior, five-minute lag under writes,
  1 GB headroom, encryption, retention, and bucket-outage behavior. The
  owner-selected working objective is explicit, but it is not adoption evidence.

## 4. Working Profile C recommendation (local preparation, not acceptance)

Use Profile C as the **working local objective**: continuous WAL-style
shipping with a five-minute lag objective, seven days of frequent recovery
points, 30 daily production versions, and 7 daily development versions.
Verify every sync and perform a daily restore to a separate target. The local
rehearsal targets a 15-minute restore-to-verified-readable-copy objective;
owner authorization is required before any cutover.

Why C over B for this preparation: the owner prefers an eager recovery point
rather than accepting a 24-hour exposure window. The planning assumption that
per-action object pricing is not the blocker does not remove storage,
transfer, sidecar CPU/RSS, WAL/object growth, restore, or monitoring costs.
Those costs and the five-minute lag remain unmeasured until the sandbox.

Why C is not accepted yet: continuous replication can stall while serving
writes continue, checkpoint contention can affect the gateway's 250ms busy
window, and a local filesystem mock cannot prove Litestream/OVH behavior.
The adoption packet must prove lag, failure visibility, resource headroom,
retention, registry coverage, encryption, and separate-target restore.

Fallback: if any adoption gate fails, block rollout and retain the existing
manual validated export/import path. A manual snapshot after a failed
continuous attempt is recovery evidence for that point, not proof that the
continuous path recovered.

## 5. Cost drivers to watch

- **Retained bytes**: sum over databases of (snapshot size × versions). Small
  SQLite files make B cheap; growth or many dev copies push toward shorter
  dev retention or explicit exclusion lists.
- **Requests and transfer**: continuous WAL/object traffic plus daily-history
  uploads and restore GETs; egress matters on restore drills, not steady state.
- **Verification compute**: checksum + `integrity_check` + trial import on
  the host or a scratch target; keep it off the serving path.
- **Operational attention**: backup-health visibility (newest verified copy
  per database, failure alerts) is part of M3 scope; an unverified schedule
  is profile A with a cron entry.

## 6. Unresolved owner decisions (explicit, blocking live work)

None of the following is decided by this brief. Live M3 backup work beyond
local preparation must wait for owner answers:

1. Accept, replace, or reject the working Profile C objectives: five-minute
   lag/RPO objective, 15-minute RTO objective, and retention per database class.
2. Monthly budget cap for backup storage/requests/egress, and which
   databases are excluded from scheduled backup.
3. Bucket region/endpoint, credential scope, and encryption requirements
   (at-rest and in-transit) for backup objects.
4. Restore-target policy: always a separate database/host, who may authorize
   a restore over an existing name, and how long rollback artifacts are kept.
5. Backup-health notification: who is notified on failure, over which sink,
   and at what cadence. The exposure half is **not** open — `GET /v1/backups`
   (per-database `fresh`/`stale`/`unknown` plus `stale_after_secs`) and
   `dg backup status` shipped 2026-09-23, per `docs/observability-contract.md`
   §2. Only the delivery side of `docs/observability-contract.md` §3 alert 4
   still has no owner.
6. Whether the sandbox evidence accepts continuous Profile C under the
   five-minute lag objective and what measured headroom is required on the 1 GB
   host.

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
