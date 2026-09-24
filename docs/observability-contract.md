# Observability contract (M3 design; §2 freshness implemented 2026-09-23)

Status: **§1 signal classes stand; §2 backup freshness is implemented** as
`GET /v1/backups` + `dg backup status` (set-membership freshness against
gateway-native versioned sets; write markers are durable across restart;
restore drill still separate), and request
pressure as `GET /v1/status` + `dg status`. Disk/RSS/alert integrations
remain future work. This contract defines the signals
needed to operate DB Gateway and a future backup sidecar. The owner-selected
working objective is continuous WAL-style shipping with a five-minute lag after
committed changes, but it is not a production guarantee. This document adds no
endpoint, metric exporter, alert integration, or live host configuration. The
first supervised micro2 deployment remains loopback-only and has no backup
sidecar. Any implementation or live rollout requires separate owner approval.

## 1. Signal classes

| Signal | Required evidence | Healthy interpretation | Failure / unknown interpretation |
| --- | --- | --- | --- |
| Service state | systemd `active`/restart state and structured stderr logs | `dg.service` is active without an unexpected restart loop | inactive, repeated restart, or missing logs is an operator alert |
| Liveness | `GET /health/live` returns `200 {"status":"ok"}` | process is serving the liveness route | this says nothing about database, disk, or backup readiness |
| Request pressure | counts/rates for `capacity_exceeded`, `busy`, and `query_deadline_exceeded` | bounded load within the documented limits | sustained growth indicates overload or a resource regression; do not silently retry |
| Disk pressure | free bytes/inodes for the data directory, WAL, staging, and recovery workspace | enough headroom for active files plus the retention policy | low disk or failed staging blocks safe writes/imports and needs operator action |
| Resource pressure | gateway RSS, SQLite/page-cache behavior, CPU, and future sidecar RSS/I/O | measurements remain inside the accepted host budget | threshold and alarm action remain owner decisions until the resource budget is complete |
| Backup freshness | newest verified copy per database, lag after committed changes, last success, last failure, and restore-test result | the newest verified copy is within the five-minute working objective after committed changes | `stale`, `failed`, or `unknown` must be visible; a scheduled job alone is not healthy |
| Restore readiness | checksum, SQLite integrity, foreign-key check, protected-table check, and exact application-data comparison | a separate-target restore passes all checks | any mismatch blocks trust in the backup and raises an incident |

`/health/live` is intentionally only liveness. It must not be relabeled as
backup or database readiness. `/v1/backups` and `dg backup status` now expose
version-set freshness, but they are not a verified restore or disk/RSS alert
system. Operators must still use the service logs, documented CLI checks, and
recovery evidence as separate signals.

## 2. Backup freshness contract

The backup freshness signal is the age of the newest verified copy, not the
start time of a scheduled job. A backup point is **verified** only after the artifact checksum, SQLite
integrity, foreign-key check, protected-table check, and application-data
comparison pass on a separate target. Record, per database:

- `last_attempt_at`, `last_success_at`, and `last_failure_at`;
- `newest_verified_copy_at` and its artifact/checksum identifier;
- age computed from the newest verified copy, not from job start time;
- failure reason, retry state, and whether the registry (`metadata.sqlite`) was
  included;
- the restore-test date and exact target/result.

For the current local working policy, alert when committed changes are not
represented by a verified copy within **300 seconds**. This is an objective,
not an accepted RPO breach threshold: the sidecar, sink, resource headroom,
and restore evidence remain unverified. Durable write markers survive gateway
restart. During idle periods, report the last committed-change marker and
last verified marker rather than declaring a healthy database stale solely
because wall time advanced. A containing backup set with no durable write
marker is `unknown`, never `fresh`; before production acceptance the safe
status remains `unknown`, not `fresh`.

## 3. Alert boundaries

Minimum future alerts are:

1. service inactive or unexpectedly restarting;
2. disk/inode headroom below the accepted operating floor;
3. sustained request pressure or deadline failures;
4. backup attempt failure, lag over 300 seconds after a committed change,
   missing database, or missing registry;
5. restore verification failure or an artifact checksum mismatch;
6. resource measurements outside the accepted host budget.

Alerts must identify the database (when applicable), last verified timestamp,
current age, failure reason, and a bounded operator action. No alert should
claim data-loss prevention, an RPO, or an RTO before the owner accepts the
corresponding profile and the sandbox/live evidence exists.

## 4. Evidence and implementation gates

Before implementation or live rollout, attach:

- the accepted profile from `docs/recovery-decisions.md`;
- the Litestream/OVH sandbox matrices from
  `docs/backup-compatibility.md` §6;
- the resource report required by `docs/resource-budget.md`;
- the local failure-injection rehearsal from
  `cargo test --locked --test recovery_rehearsal`, proving stale/failed backup
  visibility while serving writes remain available;
- a separate-target restore report covering `metadata.sqlite`, database files,
  credentials/scopes, and exact application rows.

This contract deliberately does not choose Prometheus, journald scraping,
email, a hosted monitor, or a particular API shape. Those are implementation
choices after the signal semantics and authorization boundary are accepted.
