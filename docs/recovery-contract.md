# Recovery artifact and restore-verification contract

**Document role:** Current local recovery contract
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)

Status: **local contract for snapshot drills, not a backup policy**. This
document specifies what a recovery artifact contains, how a restore is
verified on an empty target, and what counts as pass or fail. It selects no
production recovery objectives/retention, touches no remote infrastructure, and proves
nothing about continuous backup. Policy choices live in
`docs/recovery-policy.md` (canonical tiers/objectives/retention;
`docs/recovery-decisions.md` keeps the research history); tool
compatibility lives in `docs/backup-compatibility.md`.

## 1. Existing behavior → coverage and gaps

| Existing behavior (source) | What it covers | What it does NOT cover |
| --- | --- | --- |
| Export uses the SQLite online backup API into private staging (`src/store/transfers.rs`: `export_to`, `export_snapshot`) | Consistent committed snapshot during concurrent writes; no raw WAL copy | Scheduling, retention, off-host copy, or failure visibility |
| Import validates in private staging then publishes atomically (`src/store/lifecycle.rs`: `import_from`) | `PRAGMA integrity_check` must read `ok`; `pragma_foreign_key_check` must be empty; foreign `_dg_*` tables rejected; incompatible `_dg_replay` schema rejected (`src/execution.rs`: `validate_import`); live and tombstoned names refused with `database_exists`; failed imports leave no partial database | Credential/registry recovery, backup freshness, or bucket behavior |
| `initialize` ensures the canonical `_dg_replay` schema before publication (`src/execution.rs`) | Imported external SQLite files support keyed writes immediately | periodic / 256-key replay retention is per-database history, not a backup |
| Tombstone + delayed purge on delete (`docs/api.md`) | Deleted names stay unqueryable/uncreatable for periodic; purge waits for open connections | Any post-purge recovery without an independent artifact |
| Restart persistence and crash-separated replay records (`docs/architecture.md`) | Acknowledged commits survive process restart; replay rows commit in the same transaction as the write | Host/disk loss, which needs an off-host artifact |

## 2. Artifact layout

A recovery artifact is a directory holding one entry per database plus the
registry entry:

```text
YYYYMMDDTHHMMSSZ/            # e.g. 20260925T223050Z; staging dirs never match
  manifest.json
  metadata.sqlite.bak        # registry: database rows, credential hashes, scopes
  <name>.sqlite              # one standalone SQLite file per database
  <name>.sha256              # hex SHA-256 of <name>.sqlite, one per database
```

`manifest.json` shape (as written by the canonical producer,
`scripts/backup.sh`):

```json
{
  "created": "2026-09-18T00:00:00Z",
  "timestamp": "20260918T000000Z",
  "started_at": 1758153600,
  "completed_at": 1758153612,
  "registry_captured_at": 1758153610,
  "verification": "validated_before_publication",
  "tool": "dg export",
  "dg_version": "0.1.0",
  "databases": [{"name": "game_a", "file": "game_a.sqlite", "sha256": "<hex>", "size": 12345}],
  "registry": {"file": "metadata.sqlite.bak", "size": 12345, "sha256": "<hex>"}
}
```

The producer also appends one JSON line per run to
`<backup-root>/attempts.jsonl` (`ts`, `started_at`, `completed_at`,
`outcome`, `set`, `databases`, `step`) and serializes runs with a
flock-held `<backup-root>/.backup.lock`. Neither file is a set and the
status reader ignores both. Re-verify any published set later with
`dg backup verify --backup-root DIR [latest|TIMESTAMP]`.

Expectations:

- The artifact set is **per-database consistent**, not a cross-database
  atomic snapshot. Each `<name>.sqlite` is a standalone file produced by the
  online-backup export path, in DELETE journal mode as documented in
  `docs/api.md`. The manifest records the backup interval and registry capture
  time; restore rejects registry/file membership mismatches.
- Each `<name>.sha256` contains the lowercase hex SHA-256 of the exact bytes
  of `<name>.sqlite`, followed by a newline. The manifest repeats the same
  hex; mismatch between the sidecar, the manifest, and recomputed bytes is a
  failure (§5).
- The registry backup is a consistent SQLite online-backup copy of
  `metadata.sqlite` (the procedure used by `scripts/backup.sh`), or a
  raw file copy taken only after the service is stopped and all SQLite WAL
  sidecars have been checkpointed/removed. Never copy only the main registry
  file while a live or idle service may have committed WAL state. Database
  contents without the registry lose credential hashes and scope rows:
  restored database files alone do not restore who may access them.
- `_dg_replay` rows travel inside each `<name>.sqlite`. They preserve recent
  keyed-write history for that database; they do not substitute for registry
  credential rows.
- Tombstoned names are excluded: an artifact never contains a database whose
  registry row carries `deleted_at`. Restoring a tombstoned name onto a host
  where the tombstone still holds must fail with `database_exists`.

### Producer durability, permissions, and run records

The canonical producer (`scripts/backup.sh`) holds this contract;
any replacement producer must hold it too:

- **Permissions.** The backup root is mode `0700`, and every published set
  is `go-rwx` (files and directories): sets contain full database bytes plus
  registry credential hashes, so group/other must never read them.
- **Crash durability.** Publication is a single same-directory rename of a
  fully validated private stage. Before the rename the producer fsyncs every
  staged file and the staging directory; after the rename it fsyncs the
  backup-root directory. A crash therefore surfaces either the previous set
  or the complete new set, never a published set with unwritten bytes — and
  `attempts.jsonl` (fsynced per append) records which runs completed.
- **Run records.** Each `attempts.jsonl` line carries `outcome` (`ok`, or
  `error` when the run died before setting one) and the `step` reached:
  `init`, `lock`, `list`, `export:<db>`, `registry`, `manifest`, `validate`,
  `fsync`, `publish`, `prune`. A line with `outcome: ok` means the named set
  published; any other line means nothing published.
- **Retention.** After publication the producer prunes to `--keep` newest
  versioned sets (default 3). Only `YYYYMMDDTHHMMSSZ` directories match;
  staging dirs, `.backup.lock`, and `attempts.jsonl` are never pruned.
- **Credentials.** The producer authenticates to the gateway with `--key-file`
  (default `/etc/dg/owner.key`). Export is scoped-key authorized
  (docs/api.md): a read-write key may export its own database, but only the
  owner credential reaches every live database — hence the default. Key files
  never enter the published set; the registry copy carries credential hashes,
  not key material.
- **Encryption.** Export sets are plaintext SQLite plus manifest/sidecars,
  protected by owner-only filesystem permissions (`chmod 700` backup root,
  `go-rwx` staging); the set format has no at-rest encryption and the
  documented off-host leg is scp/rsync. The replication sidecar leg uses client-side
  SSE-C, measured on the wire; the at-rest half is bucket decision 3, still
  open (docs/backup-compatibility.md).
- **Bucket unavailable.** The export producer has no bucket dependency: an
  outage stops neither local snapshot publication nor the manifest chain; it
  only ages the off-host second leg. For the replication sidecar leg, bucket outage
  stops new recoverable points without stopping local writes, and the failure
  must be visible (docs/recovery-decisions.md outage windows).
- **Empty-listing refusal.** A run that lists zero live databases exits
  nonzero and publishes nothing: an empty listing is treated as a gateway
  fault, not as an empty backup.

## 3. Empty-target restoration (ordered steps)

Restore always targets an **empty data directory** (fresh `dg init` or a
scratch directory that has never registered the names). Restoring over a
live or tombstoned name is forbidden: `import_from` refuses it, and any
drill that attempts it fails.

1. Start from the artifact directory. Recompute every SHA-256 and compare
   against both the sidecar and the manifest, or run `dg backup verify
   --backup-root DIR <set>` (same checks plus foreign keys and protected
   tables). Stop on any mismatch.
2. Provision the empty target directory and its owner credential locally.
3. Pick exactly one of the two restore paths below. A drill must state
   which path it took.
4. Run integrity checks (§4), then application-data checks (§4).
5. Re-verify scoped access: a read-only key reads, a read-only key fails to
   write, and an unscoped key is denied — or record that the drill restored
   files only and skipped access verification (a gap, not a pass).

### Path A — restore the registry (credentials travel with the data)

Restore `metadata.sqlite.bak` → target `metadata.sqlite` while the target
server is stopped, after verifying the backup is a standalone SQLite file.
Never restore a main-file-only copy taken from a live WAL database. Then
place each verified `<name>.sqlite` into the stopped target data directory
and start the server. Do not import on this path: the restored registry
already holds live rows (import refuses live names with `database_exists`),
and importing would mint new generations and invalidate the restored scoped
keys. Drop any init-time WAL sidecars (`metadata.sqlite-wal/-shm`) before
starting so the restored main file opens clean.

### Path B — fresh registry (keys are re-issued)

Keep the target's own freshly initialized registry and import each
`<name>.sqlite` under its manifest name through the validated import path
(never by copying files into a running data directory). Re-issue scoped
keys on the target and record that the drill did so; original credentials
do not survive this path.

## 4. Verification checks

Integrity checks (must all pass):

- `PRAGMA integrity_check` returns exactly `ok` on every restored database.
- `SELECT count(*) FROM pragma_foreign_key_check` returns `0` on every
  restored database.
- No table outside the canonical `_dg_replay` matches `_dg_%` in
  `sqlite_master` on any restored database.
- Recomputed SHA-256 of each restored source file matches manifest and
  sidecar recorded before import.

Application-data checks (drill-specific, must be exact):

- The drill script fixes its schema and rows up front (for example: two
  tables, one foreign key, N rows with literal values across supported
  types). After restore, `SELECT` comparisons return exactly the seeded
  rows in order — counts alone are insufficient.
- The source database remains unchanged: the same queries against the
  original return the same rows after the drill.
- The restored database is independent: a write to the restored copy is
  invisible to the source and vice versa (verified by re-reading both).

## 5. Success / failure criteria for a local drill

Pass requires **all** of: SHA-256 chain matches; every database is
restored either by stopped placement of a verified file (registry-restored
path) or by validated import into an empty registry (fresh-registry path);
integrity checks pass on every restored database; application rows match
exactly; source unchanged; restored copy independent; no partial database
published on any rejected input.

Fail on **any** of: checksum/sidecar/manifest mismatch; import over a live
or tombstoned name; `integrity_check` anything but `ok`; foreign-key
violations; unexpected `_dg_*` tables; row mismatch; source modified by the
drill; rejected input (garbage bytes, incompatible `_dg_replay`, FK
violations) publishing a target; or scoped-access verification skipped but
claimed as passed.

Corrupt-input rejection without publication is a required negative case,
not optional: the drill feeds at least one invalid artifact file and proves
the target name stays absent afterwards.

## Run

```sh
timeout 120 cargo test --locked --test recovery_drill
timeout 600 cargo test --locked --test backup_pipeline
```

`recovery_drill` seeds fixed schema/rows, exports via the online-backup
path, imports under a distinct name, asserts integrity plus exact rows on
both copies, proves copy independence, and proves corrupt input publishes
nothing. `backup_pipeline` proves the versioned-set path end to end: the
producer script publishes a set the status endpoint reports fresh, `dg
backup verify` accepts it and rejects tampered/malformed sets, an
interrupted run publishes nothing and the next run recovers, failures are
recorded, and the Path A restore drill restores exact rows with
re-verified access plus tombstone-mismatch rejection.

## 6. Manual snapshot drill vs continuous backup

A passing local drill proves the **snapshot path**: one artifact, one
empty target, one verification. It does not prove continuous backup —
freshness, scheduling, retention enforcement, outage visibility, bucket
durability, restore latency at scale, or 1 GB host headroom under
replication. Claiming a drill as backup coverage is a failure of reporting,
even if every check above passed. Continuous-backup claims need the sandbox
program in `docs/backup-compatibility.md` §6 plus owner-accepted objectives
from `docs/recovery-decisions.md` §6.

## 7. Local Profile C rehearsal

`src/bin/recovery_rehearsal.rs` and `tests/recovery_rehearsal.rs` provide a
filesystem-backed object mock around the existing export/import trust
boundary. The rehearsal records the working Profile C objectives (continuous
WAL-style shipping, five-minute lag, extended frequent points, 30/7 periodic
history, every-sync verification, periodic separate-target restore, and a
periodic local recovery objective) and injects four failures:

- crash during shipping, with `failed` then `stale` status visibility;
- partial/corrupt object, with checksum/integrity rejection and no publication;
- permission/outage, with visible failure while serving writes continue; and
- disk/WAL pressure, with a held-WAL measurement and bounded sink failure.

A successful manual export is then used as the explicit fallback and restored
to a separate target with exact rows, integrity, foreign-key, and
copy-independence checks. The recorded local output is
`the measurement record This is local fault/status/restore
evidence only: it does not prove replication sidecar timing, S3 behavior, object storage provider
compatibility, a 1 GiB sidecar budget, or a production recovery objective.
