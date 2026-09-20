# Observability contract (M3 design, not implementation)

Status: **design-only, local preparation**. This contract defines the signals
needed to operate DB Gateway and a future backup sidecar. It adds no endpoint,
metric exporter, alert integration, or live host configuration. The first
supervised micro2 deployment remains loopback-only and has no backup sidecar.
Any implementation or live rollout requires separate owner approval.

## 1. Signal classes

| Signal | Required evidence | Healthy interpretation | Failure / unknown interpretation |
| --- | --- | --- | --- |
| Service state | systemd `active`/restart state and structured stderr logs | `dg.service` is active without an unexpected restart loop | inactive, repeated restart, or missing logs is an operator alert |
| Liveness | `GET /health/live` returns `200 {"status":"ok"}` | process is serving the liveness route | this says nothing about database, disk, or backup readiness |
| Request pressure | counts/rates for `capacity_exceeded`, `busy`, and `query_deadline_exceeded` | bounded load within the documented limits | sustained growth indicates overload or a resource regression; do not silently retry |
| Disk pressure | free bytes/inodes for the data directory, WAL, staging, and recovery workspace | enough headroom for active files plus the retention policy | low disk or failed staging blocks safe writes/imports and needs operator action |
| Resource pressure | gateway RSS, SQLite/page-cache behavior, CPU, and future sidecar RSS/I/O | measurements remain inside the accepted host budget | threshold and alarm action remain owner decisions until the resource budget is complete |
| Backup freshness | newest verified copy per database, age, last success, last failure, and restore-test result | the newest verified copy is within the owner-accepted objective | `stale`, `failed`, or `unknown` must be visible; a scheduled job alone is not healthy |
| Restore readiness | checksum, SQLite integrity, foreign-key check, protected-table check, and exact application-data comparison | a separate-target restore passes all checks | any mismatch blocks trust in the backup and raises an incident |

`/health/live` is intentionally only liveness. It must not be relabeled as
backup or database readiness. Until a readiness/backup API is implemented,
operators use the service logs, the documented CLI checks, and the backup
report as separate evidence.

## 2. Backup freshness contract

A backup point is **verified** only after the artifact checksum, SQLite
integrity, foreign-key check, protected-table check, and application-data
comparison pass on a separate target. Record, per database:

- `last_attempt_at`, `last_success_at`, and `last_failure_at`;
- `newest_verified_copy_at` and its artifact/checksum identifier;
- age computed from the newest verified copy, not from job start time;
- failure reason, retry state, and whether the registry (`metadata.sqlite`) was
  included;
- the restore-test date and exact target/result.

The threshold is intentionally not fixed here. `docs/recovery-decisions.md`
contains candidate objectives; the owner must accept one before an alert can
be called an RPO breach. Before that decision, the safe status is `unknown`,
not `fresh`.

## 3. Alert boundaries

Minimum future alerts are:

1. service inactive or unexpectedly restarting;
2. disk/inode headroom below the accepted operating floor;
3. sustained request pressure or deadline failures;
4. backup attempt failure, stale age, missing database, or missing registry;
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
- a local failure-injection test proving stale/failed backup visibility while
  serving writes remain available;
- a separate-target restore report covering `metadata.sqlite`, database files,
  credentials/scopes, and exact application rows.

This contract deliberately does not choose Prometheus, journald scraping,
email, a hosted monitor, or a particular API shape. Those are implementation
choices after the signal semantics and authorization boundary are accepted.
