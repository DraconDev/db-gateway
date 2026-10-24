# Observability contract (M3 design; §2 freshness implemented 2026-09-23, §3 alert 4 delivery implemented 2026-09-28)

Status: **§1 signal classes stand; §2 backup freshness is implemented** as
`GET /v1/backups` + `dg backup status` (set-membership freshness against
gateway-native versioned sets; write markers are durable across restart;
restore drill still separate), and request
pressure as `GET /v1/status` + `dg status`. **§3 alert 4 is now delivered**
per §3a below: immediate notice on a failed backup attempt plus a daily
per-database fresh/stale/unknown digest, off by default and enabled per
gateway by flag. Disk/RSS/alert integrations
remain future work. This contract defines the signals
needed to operate DB Gateway and a future backup sidecar. Backup tiers are
canonical in `docs/recovery-policy.md` (2026-10-19): continuous Litestream
WAL shipping to OVH is Tier 1 (host loss; the five-minute Profile C
objective is a working target there, not a production guarantee) and
per-database consistent verified snapshots are Tier 2 (local rollback).
This document adds no endpoint,
metric exporter, alert integration, or live host configuration. The first
supervised micro2 deployment began loopback-only with no backup sidecar;
the sidecar and hourly producer are live there now.
Any implementation or live rollout requires separate owner approval.
Related: `docs/deployment.md` (install configuration),
`docs/upgrade-rollback.md` (binary upgrades and rollback),
`docs/recovery-contract.md` (full host replacement and restore).

## 1. Signal classes

| Signal | Required evidence | Healthy interpretation | Failure / unknown interpretation |
| --- | --- | --- | --- |
| Service state | systemd `active`/restart state and structured stderr logs | `dg.service` is active without an unexpected restart loop | inactive, repeated restart, or missing logs is an operator alert |
| Liveness | `GET /health/live` returns `200 {"status":"ok"}` | process is serving the liveness route | this says nothing about database, disk, or backup readiness |
| Request pressure | counts/rates for `capacity_exceeded`, `busy`, and `query_deadline_exceeded` | bounded load within the documented limits | sustained growth indicates overload or a resource regression; do not silently retry |
| Queue waits | per-database `queue_waits`/`queue_wait_ms`/`queue_timeouts` in `GET /v1/status` | waits near zero, timeouts zero | sustained `queue_timeouts` growth means per-database writer starvation under read flood; escalate to the writer-preferring bound (scale proposal Phase 2) |
| Disk pressure | free bytes/inodes for the data directory, WAL, staging, and recovery workspace | enough headroom for active files plus the retention policy | low disk or failed staging blocks safe writes/imports and needs operator action |
| Deployment placement | the host, the bound address, the data directory, the backup root, the disk headroom and the retention, in `GET /v1/status` (`deployment`) | the gateway knows where it is and how much room is left | a gateway that cannot be located from the client (a tunnel hides the machine) is an operator question, not an operator action |
| Off-host copy | the replica target, its retention, its coverage and the sidecar's state, published by the host and reported in `GET /v1/backups` (`replica`) | a configured replica that is running, covering every live database | a configured replica that is not replicating is the failure that looks most like success from a distance |
| Resource pressure | gateway RSS, SQLite/page-cache behavior, CPU, and future sidecar RSS/I/O | measurements remain inside the accepted host budget | measured by the composed sampler series since 2026-10-19 (audit A7); thresholds are the budget's half-cap ceilings |
| Backup freshness | newest verified copy per database, lag after committed changes, last success, last failure, and restore-test result | the newest verified copy is within the selected snapshot-cadence objective after committed changes | `stale`, `failed`, or `unknown` must be visible; a scheduled job alone is not healthy |
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

For the Tier 2 snapshot design, alert when committed changes are
not represented by a verified copy within the owner-approved snapshot cadence.
The five-minute Tier 1 objective is the Profile C target in
`docs/recovery-policy.md`, not a current RPO promise.
Cadence, sink, resource headroom, and restore evidence remain policy or
verification inputs rather than guarantees. Durable write markers survive gateway
restart. During idle periods, report the last committed-change marker and
last verified marker rather than declaring a healthy database stale solely
because wall time advanced. A containing backup set with no durable write
marker is `unknown`, never `fresh`; before production acceptance the safe
status remains `unknown`, not `fresh`.

## 3. Alert boundaries

Minimum future alerts are:

1. service inactive or unexpectedly restarting;
2. disk/inode headroom below the accepted operating floor. **Delivered
   2026-10-19 (audit A8)** — `scripts/dg-alerts.sh` (disk check:
   free bytes/inodes vs floor; lag check: sidecar active, replica
   coverage, `litestream status` all-ok, WAL size ceiling) on
   `dg-alerts.timer` (15 min),
   journal-delivered like the digest. Failure drills: forced disk
   floor, oversized-WAL sandbox, and an error-row `litestream
   status` stub all fire; healthy inputs stay silent
   (`docs/audit-2026-10-19.md` A8 record). The Litestream-lag half of
   this alert is new with it — previously only freshness (§2/§3a)
   existed, nothing watched the sidecar itself;
3. sustained request pressure or deadline failures;
4. backup attempt failure, committed changes not represented by a verified
   copy within the owner-approved snapshot cadence (§2), missing database,
   or missing registry. **Delivered** — see "alert 4 delivery" below;
5. restore verification failure or an artifact checksum mismatch;
6. resource measurements outside the accepted host budget.

Alerts must identify the database (when applicable), last verified timestamp,
current age, failure reason, and a bounded operator action. No alert should
claim data-loss prevention, an RPO, or an RTO before the owner accepts the
corresponding profile and the sandbox/live evidence exists.

## 3a-0. Canonical digest mechanism (2026-10-19, audit A8)

Two digest mechanisms exist and exactly one is canonical per
deployment — running both delivers every digest twice and, worse,
lets each mask the other's failure:

- **On micro2 (and any systemd deployment): the external timer is
  canonical.** `dg-backup-digest.timer` runs `dg backup digest`
  offline against the data directory: it works with the gateway
  stopped (the in-server digest cannot report on a dead gateway —
  precisely the 3 a.m. case), it is the path the 2026-10-17 watchdog
  proved end-to-end, and it is what is enabled on micro2 today
  (verified read-only 2026-10-19: timer enabled+active, and
  `dg.service` carries no digest flags). The in-server
  `--backup-digest-secs` / `--backup-failure-poll-secs` flags must
  stay **off** (0, the default) on micro2.
- **In-server flags are for non-systemd single-process deployments**
  (a lone `dg serve` with no timer infrastructure), where they are
  the only delivery path. Choosing them there is legitimate; mixing
  them with the external timer is not a configuration, it is a bug.

## 3a. Alert 4 delivery: sink and cadence

Owner decision 2026-09-27 (docs/recovery-decisions.md §6.5): immediate notice
on failure plus a daily digest of per-database fresh/stale/unknown. The
exposure half is §2 above; this is the delivery half, and it is what the
decision called the floor, because no alert stack exists in the project.

| | value | where it is set |
| --- | --- | --- |
| Digest sink | one structured log line per notification, always on once delivery is enabled | `LogSink` (src/notify.rs); systemd captures stderr |
| Second sink | POST the note as JSON to an owner-supplied URL: HTTPS, or plain HTTP on a literal loopback address; no proxy, no redirects | `dg serve --backup-notify-webhook URL` |
| Digest cadence | 24h; the first digest goes out one full period after startup, never on boot | `dg serve --backup-digest-secs N`, **default 0 (off)**; on micro2 the external `dg-backup-digest.timer` delivers the cadence instead (§3a-0) — never both |
| Immediate-failure notice | one notice per failed attempt, as soon as the producer records it; polled every 60s | `dg serve --backup-failure-poll-secs N`, **default 0 (off)** |
| Failure signal | `$BACKUP_ROOT/attempts.jsonl`, appended by `scripts/micro2-backup.sh`: outcome, step reached, set | `read_attempts` (src/backup_health.rs) |
| Notice scope | the newest failed attempt not yet reported, once. Two failures inside one poll window produce one notice naming the newer; the complete history stays in `attempts.jsonl` and the next failure always alerts again | `DeliveryState` (src/notify.rs) |
| Freshness rule | §2, computed once and shared by `/v1/backups`, `dg backup status`, `dg backup digest` and the digest sink | `snapshot` (src/backup_health.rs) |
| On demand | `dg backup digest [--json]`, reads the data directory directly, so it works with the gateway stopped; exits nonzero when any database is stale or unknown | owner-only equivalent of the delivered digest |
| Delivery is opt-in | no flag, no notification anywhere: both cadences default to 0 | the decision was delivery, not a new default behaviour for every gateway |

The owner's daily-digest decision is therefore this one flag set, with the
cadence the decision names:

```sh
dg serve --backup-digest-secs 86400 --backup-failure-poll-secs 60 \
         [--backup-notify-webhook https://…/hook]
```

With `--backup-digest-secs 0` only the immediate failure notice runs, and
with `--backup-failure-poll-secs 0` only the digest runs. Either is a
legitimate configuration; neither is a degraded one.

Delivery is off unless a flag asks for it, so a gateway that nobody opted in
sends nothing anywhere and no existing deployment changes behaviour. Once
enabled, the log sink is unconditional because structured logging is already
the service's operator channel, and it is the sink that cannot quietly stop
working — a webhook returning 500 is a reported delivery failure, not a
silent loss, and it never blocks the other sinks.

What the delivered note carries, per §3: the database, its state, the newest
verified set, the age, the failing attempt and the step it reached, and a
bounded operator action. What it must never carry: an RPO, an RTO, or a
data-loss claim. `Note` has no field for any of them, so there is nowhere to
put one — none is accepted yet.

What the note does **not** carry, so §2 is not over-read: §2 asks for
per-database `last_attempt_at`, `last_success_at` and the restore-test date.
Those are still not recorded. The note reports the attempt log's newest
attempt and newest failure at the run level, plus per-database freshness from
set membership. Closing the per-database attempt history and the restore-test
date is remaining §2 work, not something this delivery claims.

What delivery is *not*: coverage evidence. A digest says a database is fresh
against the cadence in §2; it is not a restore, and a passing digest does not
close the restore-verification gate in §5.

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
Alert 4 is the one exception, and it is bounded on purpose: §3a chooses a
structured-log sink plus an optional HTTPS/loopback webhook for backup health
only, because the owner's 2026-09-27 decision asked for delivery and the
project had no alert stack to choose from. Alerts 1, 2, 3, 5 and 6 remain
unchosen, and no monitor, pager or hosted alerting is implied by §3a.
