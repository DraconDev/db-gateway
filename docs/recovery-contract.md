# Recovery artifact and restore-verification contract

**Document role:** Current local recovery contract
**Canonical roadmap:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)

Status: **local contract for snapshot drills, not a backup policy**. This
document specifies what a recovery artifact contains, how a restore is
verified on an empty target, and what counts as pass or fail. It selects no
production RPO/RTO/retention, touches no remote infrastructure, and proves
nothing about continuous backup. Policy choices live in
`docs/recovery-decisions.md`; tool compatibility lives in
`docs/backup-compatibility.md`.

## 1. Existing behavior → coverage and gaps

| Existing behavior (source) | What it covers | What it does NOT cover |
| --- | --- | --- |
| Export uses the SQLite online backup API into private staging (`src/store.rs`: `export_to`, `export_snapshot`) | Consistent committed snapshot during concurrent writes; no raw WAL copy | Scheduling, retention, off-host copy, or failure visibility |
| Import validates in private staging then publishes atomically (`src/store.rs`: `import_from`) | `PRAGMA integrity_check` must read `ok`; `pragma_foreign_key_check` must be empty; foreign `_dg_*` tables rejected; incompatible `_dg_replay` schema rejected (`src/execution.rs`: `validate_import`); live and tombstoned names refused with `database_exists`; failed imports leave no partial database | Credential/registry recovery, backup freshness, or bucket behavior |
| `initialize` ensures the canonical `_dg_replay` schema before publication (`src/execution.rs`) | Imported external SQLite files support keyed writes immediately | 24h / 256-key replay retention is per-database history, not a backup |
| Tombstone + delayed purge on delete (`docs/api.md`) | Deleted names stay unqueryable/uncreatable for 24h; purge waits for open connections | Any post-purge recovery without an independent artifact |
| Restart persistence and crash-separated replay records (`docs/architecture.md`) | Acknowledged commits survive process restart; replay rows commit in the same transaction as the write | Host/disk loss, which needs an off-host artifact |

## 2. Artifact layout

A recovery artifact is a directory holding one entry per database plus the
registry entry:

```text
recovery-YYYYMMDDTHHMMSSZ/
  manifest.json
  metadata.sqlite.bak        # registry: database rows, credential hashes, scopes
  <name>.sqlite              # one standalone SQLite file per database
  <name>.sha256              # hex SHA-256 of <name>.sqlite, one per database
```

`manifest.json` shape:

```json
{
  "created": "2026-09-18T00:00:00Z",
  "tool": "dg export",
  "databases": [{"name": "game_a", "file": "game_a.sqlite", "sha256": "<hex>", "size": 12345}],
  "registry": {"file": "metadata.sqlite.bak", "sha256": "<hex>"}
}
```

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
  `metadata.sqlite` (the procedure used by `scripts/micro2-backup.sh`), or a
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

## 3. Empty-target restoration (ordered steps)

Restore always targets an **empty data directory** (fresh `dg init` or a
scratch directory that has never registered the names). Restoring over a
live or tombstoned name is forbidden: `import_from` refuses it, and any
drill that attempts it fails.

1. Start from the artifact directory. Recompute every SHA-256 and compare
   against both the sidecar and the manifest. Stop on any mismatch.
2. Provision the empty target directory and its owner credential locally.
3. Restore the registry entry first (`metadata.sqlite.bak` → target
   `metadata.sqlite`) while the target server is stopped, after verifying the
   backup is a standalone SQLite file, or re-issue keys and record that the
   drill did so. Never restore a main-file-only copy taken from a live WAL
   database. A drill must state which path it took.
4. Import each `<name>.sqlite` under its manifest name through the validated
   import path (never by copying files into a running data directory).
5. Run integrity checks (§4), then application-data checks (§4).
6. Re-verify scoped access: a read-only key reads, a read-only key fails to
   write, and an unscoped key is denied — or record that the drill restored
   files only and skipped access verification (a gap, not a pass).

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

Pass requires **all** of: SHA-256 chain matches; every import targets an
empty directory and succeeds; integrity checks pass on every restored
database; application rows match exactly; source unchanged; restored copy
independent; no partial database published on any rejected input.

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
```

The test seeds fixed schema/rows, exports via the online-backup path,
imports under a distinct name, asserts integrity plus exact rows on both
copies, proves copy independence, and proves corrupt input publishes
nothing.

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
WAL-style shipping, five-minute lag, 7-day frequent points, 30/7 daily
history, every-sync verification, daily separate-target restore, and a
15-minute local RTO objective) and injects four failures:

- crash during shipping, with `failed` then `stale` status visibility;
- partial/corrupt object, with checksum/integrity rejection and no publication;
- permission/outage, with visible failure while serving writes continue; and
- disk/WAL pressure, with a held-WAL measurement and bounded sink failure.

A successful manual export is then used as the explicit fallback and restored
to a separate target with exact rows, integrity, foreign-key, and
copy-independence checks. The recorded local output is
`docs/recovery-rehearsal-evidence.json`. This is local fault/status/restore
evidence only: it does not prove Litestream timing, S3 behavior, OVH
compatibility, a 1 GiB sidecar budget, or a production RTO.
