//! Filesystem-backed recovery rehearsal for the local Profile C contract.
//!
//! This is a deterministic fault-injection harness around the gateway's
//! SQLite online-backup/export and validated-import paths. It is not a
//! Litestream, S3, OVH, micro2, or continuous-replication test.

use dg::{
    execution::{self, Statement},
    store::Store,
};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

type AnyResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy)]
enum Fault {
    None,
    CrashDuringShipping,
    CorruptObject,
    PermissionOrOutage,
    DiskPressure,
}

struct LocalObjectStore {
    root: PathBuf,
    verified_marker: Option<i64>,
    last_failure: Option<&'static str>,
    quota_bytes: Option<u64>,
}

impl LocalObjectStore {
    fn new(root: &Path) -> AnyResult<Self> {
        fs::create_dir_all(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            verified_marker: None,
            last_failure: None,
            quota_bytes: None,
        })
    }

    fn object_path(&self) -> PathBuf {
        self.root.join("primary.sqlite")
    }

    fn checksum_path(&self) -> PathBuf {
        self.root.join("primary.sqlite.sha256")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    fn fail(&mut self, reason: &'static str) -> String {
        self.last_failure = Some(reason);
        reason.to_string()
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        let temporary = self.root.join(".primary.sqlite.upload");
        let result = (|| {
            let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
            file.write_all(bytes).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            fs::rename(&temporary, path).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn write_manifest(&self, expected: &str, marker: i64, size: usize) -> Result<(), String> {
        let manifest = json!({
            "schema": "dg-local-object/1",
            "file": "primary.sqlite",
            "sha256": expected,
            "size": size,
            "source_marker": marker,
        });
        fs::write(
            self.manifest_path(),
            serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }

    fn ship(&mut self, source: &Path, marker: i64, fault: Fault) -> Result<(), String> {
        let bytes = fs::read(source)
            .map_err(|error| self.fail("source_read_failed") + &format!(": {error}"))?;
        let expected = sha256(&bytes);
        match fault {
            Fault::PermissionOrOutage => return Err(self.fail("sink_unavailable")),
            Fault::DiskPressure => {
                if self.quota_bytes.unwrap_or(u64::MAX) < bytes.len() as u64 {
                    return Err(self.fail("disk_or_wal_pressure"));
                }
            }
            Fault::CrashDuringShipping => {
                let partial = self.root.join(".primary.sqlite.upload.partial");
                // About half the bytes, never more than the snapshot holds.
                // The old `.max(2) / 2` guarded a 1-byte input but not a
                // 0-byte one, so `&bytes[..1]` panicked the drill.
                let half = bytes.len().div_ceil(2).min(bytes.len());
                let mut file = File::create(partial).map_err(|error| error.to_string())?;
                file.write_all(&bytes[..half])
                    .map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())?;
                return Err(self.fail("crash_during_shipping"));
            }
            Fault::CorruptObject => {
                let mut corrupt = bytes;
                // A 0-byte snapshot has no byte to flip, and that absence is
                // the integrity failure this drill records — not a panic.
                // Same guard the CrashDuringShipping branch above carries.
                let index = corrupt.len() / 2;
                let Some(byte) = corrupt.get_mut(index) else {
                    return Err(self.fail("checksum_or_integrity_failure"));
                };
                *byte ^= 0x01;
                self.write_atomic(&self.object_path(), &corrupt)?;
                fs::write(self.checksum_path(), format!("{expected}\n"))
                    .map_err(|error| error.to_string())?;
                self.write_manifest(&expected, marker, corrupt.len())?;
                return Err(self.fail("checksum_or_integrity_failure"));
            }
            Fault::None => {}
        }

        self.write_atomic(&self.object_path(), &bytes)?;
        fs::write(self.checksum_path(), format!("{expected}\n"))
            .map_err(|error| error.to_string())?;
        self.write_manifest(&expected, marker, bytes.len())?;
        self.verified_marker = Some(marker);
        self.last_failure = None;
        Ok(())
    }

    fn clear_failure(&mut self) {
        self.last_failure = None;
    }

    fn status(&self, current_marker: i64) -> Value {
        let state = if let Some(reason) = self.last_failure {
            json!({"state":"failed", "reason":reason})
        } else if self.verified_marker == Some(current_marker) {
            json!({"state":"fresh"})
        } else if self.verified_marker.is_some() {
            json!({"state":"stale"})
        } else {
            json!({"state":"unknown"})
        };
        json!({
            "verified_marker": self.verified_marker,
            "current_marker": current_marker,
            "last_failure": self.last_failure,
            "status": state,
        })
    }

    fn archive_registry(&self, source_root: &Path) -> AnyResult<Value> {
        let source = source_root.join("metadata.sqlite");
        // The source Store IS still live here and holds its own metadata
        // connection, so the registry copy cannot rely on an adjacent WAL file
        // being present in the object mock. Open the file directly, force a
        // TRUNCATE checkpoint, and only then copy the single main file, so
        // the archive is self-contained by construction rather than by
        // assuming no other connection is around.
        let source_conn = Connection::open(&source)?;
        source_conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(source_conn);
        let destination = self.root.join("metadata.sqlite.bak");
        copy_sync(&source, &destination)?;
        let digest = sha256(&fs::read(&destination)?);
        fs::write(
            self.root.join("metadata.sqlite.bak.sha256"),
            format!("{digest}\n"),
        )?;
        let registry = Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String =
            registry.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        require(
            integrity == "ok",
            "registry integrity_check did not return ok",
        )?;
        let database_rows: i64 =
            registry.query_row("SELECT count(*) FROM databases", [], |row| row.get(0))?;
        let credential_rows: i64 =
            registry.query_row("SELECT count(*) FROM credentials", [], |row| row.get(0))?;
        Ok(json!({
            "file": "metadata.sqlite.bak",
            "sha256": digest,
            "verified": true,
            "integrity": "ok",
            "database_rows": database_rows,
            "credential_rows": credential_rows,
        }))
    }

    fn verify_object(&self) -> AnyResult<()> {
        let bytes = fs::read(self.object_path())?;
        let expected = fs::read_to_string(self.checksum_path())?;
        let expected = expected.trim();
        let actual = sha256(&bytes);
        if actual != expected {
            return Err(format!("checksum mismatch: expected {expected}, got {actual}").into());
        }
        execution::validate_import(&self.object_path())?;
        Ok(())
    }

    fn rejected_restore(&self) -> AnyResult<Value> {
        let checksum_error = self
            .verify_object()
            .expect_err("corrupt object must fail checksum or import validation");
        let target_dir = tempfile::tempdir()?;
        let target = Store::open(target_dir.path())?;
        target.bootstrap()?;
        let import_error = target
            .import_from("restored", &self.object_path())
            .expect_err("corrupt object must not publish");
        let published = target.path("restored").is_ok();
        if published {
            return Err("corrupt object was published".into());
        }
        Ok(json!({
            "published": false,
            "checksum_or_integrity_error": checksum_error.to_string(),
            "import_error": import_error.to_string(),
        }))
    }

    fn restore_and_verify(&self, source: &Store, expected: &Value) -> AnyResult<Value> {
        self.verify_object()?;
        let started = Instant::now();
        let target_dir = tempfile::tempdir()?;
        let target = Store::open(target_dir.path())?;
        target.bootstrap()?;
        target.import_from("restored", &self.object_path())?;
        let restored_path = target.path("restored")?;
        integrity_ok(&restored_path)?;
        let restored_rows = rows_path(&restored_path)?;
        if &restored_rows != expected {
            return Err(
                format!("restored rows differ: expected {expected}, got {restored_rows}").into(),
            );
        }
        insert_event_named(&target, "restored", 2_000_000, "restored-only")?;
        let source_rows = rows_store(source)?;
        if &source_rows != expected {
            return Err("restored copy modified the source".into());
        }
        Ok(json!({
            "rto_ms": started.elapsed().as_millis(),
            "integrity": "ok",
            "foreign_keys": 0,
            "exact_rows": true,
            "independent": true,
            "target": "separate-empty-store",
        }))
    }
}

fn copy_sync(source: &Path, destination: &Path) -> AnyResult<()> {
    let bytes = fs::read(source)?;
    let mut file = File::create(destination)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn require(condition: bool, message: &str) -> AnyResult<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn insert_event_named(store: &Store, name: &str, id: i64, body: &str) -> AnyResult<()> {
    let path = store.path(name)?;
    execution::execute(
        &path,
        &[Statement {
            sql: "INSERT INTO events (id, body) VALUES (?, ?)".into(),
            args: vec![json!(id), json!(body)],
        }],
        false,
        false,
        "owner",
        None,
    )?;
    Ok(())
}

fn insert_event(store: &Store, id: i64, body: &str) -> AnyResult<()> {
    insert_event_named(store, "primary", id, body)
}

fn rows_path(path: &Path) -> AnyResult<Value> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare("SELECT id, body FROM events ORDER BY id")?;
    let rows = statement
        .query_map([], |row| {
            Ok(json!([row.get::<_, i64>(0)?, row.get::<_, String>(1)?]))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!(rows))
}

fn rows_store(store: &Store) -> AnyResult<Value> {
    rows_path(&store.path("primary")?)
}

fn integrity_ok(path: &Path) -> AnyResult<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    require(integrity == "ok", "integrity_check did not return ok")?;
    let foreign_keys: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    require(foreign_keys == 0, "foreign_key_check returned violations")?;
    Ok(())
}

fn snapshot_and_ship(
    store: &Store,
    sink: &mut LocalObjectStore,
    marker: i64,
    fault: Fault,
) -> AnyResult<Result<(), String>> {
    let snapshot = store.export_snapshot("primary")?;
    Ok(sink.ship(snapshot.path(), marker, fault))
}

fn create_wal_pressure(path: &Path) -> AnyResult<u64> {
    let reader = Connection::open(path)?;
    reader.execute_batch("PRAGMA journal_mode=WAL; BEGIN;")?;
    let _: i64 = reader.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?;

    let mut writer = Connection::open(path)?;
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")?;
    let tx = writer.transaction()?;
    let payload = "w".repeat(4096);
    for id in 10_000..10_200 {
        tx.execute(
            "INSERT INTO events (id, body) VALUES (?, ?)",
            params![id, payload],
        )?;
    }
    tx.commit()?;
    drop(writer);
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let bytes = fs::metadata(&wal)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    reader.execute_batch("ROLLBACK;")?;
    Ok(bytes)
}

fn main() -> AnyResult<()> {
    let source_dir = tempfile::tempdir()?;
    let sink_dir = tempfile::tempdir()?;
    let source = Store::open(source_dir.path())?;
    source.bootstrap()?;
    source.create("primary")?;
    execution::execute(
        &source.path("primary")?,
        &[Statement {
            sql: "CREATE TABLE events (id INTEGER PRIMARY KEY, body TEXT NOT NULL)".into(),
            args: vec![],
        }],
        false,
        false,
        "owner",
        None,
    )?;
    for (id, body) in [(1, "alpha"), (2, "bravo"), (3, "charlie")] {
        insert_event(&source, id, body)?;
    }
    let initial_rows = rows_store(&source)?;
    let mut sink = LocalObjectStore::new(sink_dir.path())?;
    let registry = sink.archive_registry(source_dir.path())?;

    require(
        snapshot_and_ship(&source, &mut sink, 3, Fault::None)?.is_ok(),
        "baseline snapshot did not publish",
    )?;
    let baseline_status = sink.status(3);
    require(
        baseline_status["status"]["state"] == "fresh",
        "baseline snapshot was not fresh",
    )?;
    let baseline_restore = sink.restore_and_verify(&source, &initial_rows)?;

    insert_event(&source, 4, "crash-window")?;
    let crash_result = snapshot_and_ship(&source, &mut sink, 4, Fault::CrashDuringShipping)?;
    require(crash_result.is_err(), "crash fault unexpectedly succeeded")?;
    let crash_failed = sink.status(4);
    require(
        crash_failed["status"]["state"] == "failed",
        "crash failure was not visible",
    )?;
    sink.clear_failure();
    let crash_stale = sink.status(4);
    require(
        crash_stale["status"]["state"] == "stale",
        "stale verified point was not visible after a shipping crash",
    )?;

    insert_event(&source, 5, "corruption-window")?;
    let corrupt_result = snapshot_and_ship(&source, &mut sink, 5, Fault::CorruptObject)?;
    require(
        corrupt_result.is_err(),
        "corrupt fault unexpectedly succeeded",
    )?;
    let corrupt_status = sink.status(5);
    require(
        corrupt_status["status"]["state"] == "failed",
        "corrupt-object failure was not visible",
    )?;
    let corrupt_restore = sink.rejected_restore()?;
    sink.clear_failure();

    insert_event(&source, 6, "outage-window")?;
    let outage_result = snapshot_and_ship(&source, &mut sink, 6, Fault::PermissionOrOutage)?;
    require(
        outage_result.is_err(),
        "outage fault unexpectedly succeeded",
    )?;
    insert_event(&source, 7, "writes-continue")?;
    let outage_status = sink.status(7);
    require(
        outage_status["status"]["state"] == "failed",
        "outage failure was not visible",
    )?;
    let writes_after_outage = rows_store(&source)?;
    require(
        writes_after_outage.as_array().map_or(0, Vec::len) == 7,
        "serving writes did not continue after sink outage",
    )?;
    sink.clear_failure();

    let wal_bytes = create_wal_pressure(&source.path("primary")?)?;
    let disk_marker = rows_store(&source)?.as_array().map_or(0, Vec::len) as i64;
    // The quota forces the sink to reject deterministically so the fault PATH
    // is exercised. It says nothing about the scenario the drill is named for,
    // so the real evidence -- WAL growth while a reader holds the database --
    // has to be asserted here. It was previously only printed into the report,
    // which meant a regression that stopped growing the WAL would still emit
    // "passed": true.
    require(
        wal_bytes > 0,
        "holding a reader produced no WAL growth, so the disk/wal-pressure drill proved nothing",
    )?;
    sink.quota_bytes = Some(1);
    let disk_result = snapshot_and_ship(&source, &mut sink, disk_marker, Fault::DiskPressure)?;
    require(
        disk_result.is_err(),
        "disk-pressure fault unexpectedly succeeded",
    )?;
    let disk_status = sink.status(disk_marker);
    require(
        disk_status["status"]["state"] == "failed",
        "disk/WAL pressure failure was not visible",
    )?;
    sink.quota_bytes = None;

    // Manual snapshot fallback: clear the local sink fault and publish a fresh
    // validated export without pretending the failed continuous path recovered.
    let fallback = snapshot_and_ship(&source, &mut sink, disk_marker, Fault::None)?;
    require(fallback.is_ok(), "manual snapshot fallback did not publish")?;
    let fallback_status = sink.status(disk_marker);
    require(
        fallback_status["status"]["state"] == "fresh",
        "manual snapshot fallback was not fresh",
    )?;
    let final_rows = rows_store(&source)?;
    let fallback_restore = sink.restore_and_verify(&source, &final_rows)?;

    let report = json!({
        "schema": "dg-recovery-rehearsal/1",
        "scope": "local filesystem object mock only",
        "policy": {
            "profile": "C",
            "cadence": "continuous WAL-style shipping",
            "lag_objective_seconds": 300,
            "frequent_retention_days": 7,
            "daily_retention_days": {"production": 30, "development": 7},
            "verification": "every sync plus daily separate-target restore",
            "rto_objective_seconds": 900,
            "restore_authorization": "owner; separate target before cutover",
            "failed_gate_fallback": "block rollout; retain manual snapshots",
        },
        "registry_artifact": registry,
        "baseline": {
            "verified": true,
            "status": baseline_status,
            "restore": baseline_restore,
        },
        "scenarios": [
            {
                "name": "crash_during_shipping",
                "passed": true,
                "failed_status": crash_failed,
                "stale_status_after_failure_clear": crash_stale,
            },
            {
                "name": "partial_or_corrupt_object",
                "passed": true,
                "failed_status": corrupt_status,
                "restore_rejected_without_publication": corrupt_restore,
            },
            {
                "name": "permission_or_outage",
                "passed": true,
                "failed_status": outage_status,
                "writes_after_outage": true,
            },
            {
                "name": "disk_or_wal_pressure",
                "passed": true,
                "wal_bytes_while_reader_held": wal_bytes,
                "quota_bytes": 1,
                "failed_status": disk_status,
            },
        ],
        "fallback": {
            "mode": "manual export snapshot",
            "passed": true,
            "fresh_status": fallback_status,
            "restore": fallback_restore,
        },
        "limitations": [
            "This does not run Litestream or prove continuous shipping timing.",
            "This does not contact OVH, Turso, provider credentials, production data, or micro2.",
            "The RTO value is a local disposable measurement, not a production guarantee.",
            "The five-minute lag objective still requires a real sidecar/resource sandbox before adoption.",
        ],
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_object_on_empty_snapshot_records_failure_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = LocalObjectStore::new(dir.path()).unwrap();
        let source = dir.path().join("empty.sqlite");
        fs::write(&source, b"").unwrap();
        // The old `corrupt[len/2] ^= 1` indexed an empty buffer and aborted
        // the drill with a panic instead of recording the integrity failure.
        let result = sink.ship(&source, 7, Fault::CorruptObject);
        assert_eq!(result, Err("checksum_or_integrity_failure".to_string()));
        assert_eq!(sink.last_failure, Some("checksum_or_integrity_failure"));
    }
}
