//! On-demand backup sets: the gateway writes one NOW.
//!
//! The host runs an hourly producer (`scripts/micro2-backup.sh`) that
//! publishes versioned backup sets into the backup root. That producer
//! owns the schedule, the retention and the attempt ledger. This module
//! is the other half: the same set, written on demand, from inside the
//! gateway, so an operator does not have to wait for the next hourly run
//! to have a copy of what is live right now.
//!
//! The set this module writes is INDISTINGUISHABLE from the producer's:
//! same directory layout, same manifest, same checksums, same
//! verification. The producer's own verifier (`dg backup verify`, and
//! `crate::backup::verify_set` behind it) accepts what this module
//! publishes, and this module refuses to publish anything that verifier
//! would reject.
//!
//! ## The layout
//!
//! `<backup_root>/<timestamp>/`, where `<timestamp>` is `YYYYMMDDTHHMMSSZ`
//! in UTC at second resolution, holding
//!
//! * one `<database>.sqlite` per live database, each a consistent copy
//!   taken with the SQLite online backup API;
//! * one `<database>.sha256` sidecar per database, holding that file's
//!   sha256 and nothing else;
//! * `metadata.sqlite.bak`, the registry, copied the same way;
//! * `manifest.json`, the record of what the set holds.
//!
//! ## What this module does NOT do
//!
//! * **It does not prune.** The hourly producer owns retention: it prunes
//!   to its own `--keep` count. A run here publishes ONE set and stops,
//!   so it can never delete a set the operator wanted.
//! * **It does not write `attempts.jsonl`.** That ledger is the
//!   producer's attempt log. A second writer interleaving into it is a
//!   hazard, and the next producer run reports its own attempt anyway.
//!
//! ## The lock
//!
//! The producer takes `flock -n` on `<backup_root>/.backup.lock`. This
//! module takes the SAME lock, exclusively and non-blocking, so the two
//! producers exclude each other: a run that cannot take the lock fails
//! with a clear error instead of waiting, and never proceeds unlocked.
//! `File::try_lock` is `flock(2)`, the same primitive the shell's
//! `flock` uses, so the exclusion is real across the two.
//!
//! ## Stage, validate, publish
//!
//! The set is built in `<backup_root>/.staging.<random>`, validated
//! there, and published with a RENAME of the staging directory onto
//! `<backup_root>/<timestamp>`. A rename is atomic, so a reader never
//! sees a half-written set, and the set's name appears only once every
//! byte of it is on disk and verified.
//!
//! ## What a set is a snapshot OF
//!
//! Each database in the set is a consistent snapshot of THAT database,
//! taken with SQLite's online backup API, which is what makes the file
//! a valid database rather than a torn copy. The databases in one set
//! are not at the same instant: the producer's sets are not either,
//! because it exports each database separately too. A set is a set of
//! per-database snapshots, and the manifest records when each was
//! taken.
//!
//! While a run is in flight it holds ONE of the service's write
//! permits, so it counts against the write capacity the way any other
//! writer does; the other permits stay free and reads keep flowing on
//! the separate read lane.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::store::{ACQUIRE_TIMEOUT, MAX_TRANSFER_SIZE, Principal, Store, unauthorized};

/// The registry copy's name inside a set.
const REGISTRY_FILE: &str = "metadata.sqlite.bak";
/// The manifest's name inside a set.
const MANIFEST_FILE: &str = "manifest.json";
/// The lock both producers take, inside the backup root.
const LOCK_FILE: &str = ".backup.lock";
/// What the manifest says about how the set was checked.
const VERIFICATION: &str = "validated_before_publication";
/// The manifest's `tool` field. The producer writes `dg export` because
/// that is how it captures a database; this module captures the same way
/// (the SQLite online backup API), so the field says the same thing and
/// a set is not distinguishable by its manifest.
const TOOL: &str = "dg export";

/// One database as the published manifest records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRunDatabase {
    /// The database's name in the gateway.
    pub name: String,
    /// The file inside the set, relative to the set's directory.
    pub file: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's sha256, in lowercase hex.
    pub sha256: String,
}

/// The registry copy as the published manifest records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRunRegistry {
    /// The file inside the set, relative to the set's directory.
    pub file: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's sha256, in lowercase hex.
    pub sha256: String,
}

/// What one run published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRunOutcome {
    /// The set's directory name: `YYYYMMDDTHHMMSSZ`.
    pub set: String,
    /// The same moment, rendered as `YYYY-MM-DDTHH:MM:SSZ`.
    pub created: String,
    /// The conservative freshness boundary, in unix seconds. Captured
    /// before anything was listed or copied: a write that commits after
    /// this instant cannot be proven included in the set.
    pub started_at: u64,
    /// When the capture finished, in unix seconds.
    pub completed_at: u64,
    /// When the registry copy was taken, in unix seconds.
    pub registry_captured_at: u64,
    /// The databases the set carries.
    pub databases: Vec<BackupRunDatabase>,
    /// The registry copy the set carries.
    pub registry: BackupRunRegistry,
    /// The bytes the set holds: the databases plus the registry.
    pub bytes: u64,
    /// How long the run took, in milliseconds.
    pub duration_ms: u64,
    /// The verification the manifest records.
    pub verification: String,
}

/// A point at which a run can be made to fail on purpose.
///
/// This exists so the failure path can be PROVED rather than assumed: a
/// run that fails must leave no published set and no staging directory
/// behind, and there is no way to make a real run fail at a chosen
/// moment without corrupting the very data it is copying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaultPoint {
    /// No fault: the run behaves normally.
    #[default]
    None,
    /// Fail right after the databases are staged.
    AfterDatabases,
    /// Fail right after the registry copy is staged.
    AfterRegistry,
    /// Replace a staged database with bytes that are not a database, so
    /// the validation refuses the set.
    CorruptDatabase,
}

/// Take a backup set now and publish it.
///
/// The set is written in the shape the hourly producer writes, under the
/// same lock, with the same validation, and it is published by an atomic
/// rename. It does NOT prune: retention belongs to the producer, and a
/// run here publishes one set and stops.
///
/// The caller must be the owner: a set carries every database in the
/// deployment, so it is operator information and no scoped credential may
/// ask for one.
pub fn run(store: &Store, principal: &Principal) -> Result<BackupRunOutcome> {
    run_with_fault(store, principal, FaultPoint::None)
}

/// [`run`], with a deliberate failure point for tests.
pub fn run_with_fault(
    store: &Store,
    principal: &Principal,
    fault: FaultPoint,
) -> Result<BackupRunOutcome> {
    // Owner-only: the set carries every database, and the credential that
    // asks for it writes to the backup root.
    principal.owner()?;
    let root = store.backup_root().ok_or_else(|| {
        Error::new(
            409,
            "backup_root_unavailable",
            "this gateway has no backup root configured, so there is nowhere to publish a backup set",
        )
    })?;
    // The freshness boundary is taken BEFORE anything is listed or
    // copied. The gateway's backup status compares durable write markers
    // against this value, so it has to be conservative: a write that
    // commits after this instant is not provably in the set.
    let started = SystemTime::now();
    let started_at = unix_secs(started);
    let (year, month, day, hour, minute, second) = civil_from_epoch(started_at);
    let timestamp = format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z");
    let created = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z");

    prepare_root(&root)?;
    // The lock is the producer's own lock, taken the same way: exclusive,
    // non-blocking, and a refusal to wait. A run that cannot take it
    // writes nothing at all.
    let _lock = lock_backup_root(&root)?;

    let names = store.list(principal)?;
    if names.is_empty() {
        // The producer refuses an empty backup too, and a set with no
        // databases is a set the gateway's own verifier rejects.
        return Err(Error::new(
            409,
            "backup_empty",
            "refusing to publish an empty backup set: there are no live databases to capture",
        ));
    }

    let mut staging = Staging::create(&root)?;
    let started_clock = Instant::now();
    let (databases, registry, registry_captured_at) = capture(store, &staging, &names, fault)?;
    let completed_at = unix_secs(SystemTime::now());
    let bytes = databases.iter().map(|database| database.size).sum::<u64>() + registry.size;

    let manifest = manifest_json(
        &created,
        &timestamp,
        started_at,
        completed_at,
        registry_captured_at,
        &databases,
        &registry,
    );
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| Error::internal())?;
    // The producer writes the manifest with a trailing newline; a set
    // whose manifest differs by a byte is a set a strict reader may
    // reject.
    manifest_bytes.push(b'\n');
    let manifest_path = staging.path.join(MANIFEST_FILE);
    write_file(&manifest_path, &manifest_bytes)?;

    // Everything is on disk and checked BEFORE the rename: a set that
    // fails validation is removed and the run fails, and nothing is
    // published.
    validate(&staging, &manifest)?;

    // Durability: every staged byte is flushed before the set is
    // published, and the backup root is flushed after the rename, so a
    // crash cannot surface a published set with unwritten bytes. The
    // database copies are written by SQLite, so they are flushed here
    // rather than by the copy that made them.
    sync_staging(&staging)?;

    let destination = root.join(&timestamp);
    if std::fs::symlink_metadata(&destination).is_ok() {
        return Err(Error::with_message(
            409,
            "backup_set_exists",
            format!("a backup set named {timestamp} already exists"),
        ));
    }
    std::fs::rename(&staging.path, &destination).map_err(|_| Error::internal())?;
    staging.published = true;
    sync_directory(&root)?;

    Ok(BackupRunOutcome {
        set: timestamp,
        created,
        started_at,
        completed_at,
        registry_captured_at,
        databases,
        registry,
        bytes,
        duration_ms: u64::try_from(started_clock.elapsed().as_millis()).unwrap_or(u64::MAX),
        verification: VERIFICATION.to_string(),
    })
}

/// The router for the on-demand backup set.
///
/// `POST /v1/backups/run` publishes one set and answers with what it
/// holds. The endpoint is owner-only and needs a credential that can
/// write: it writes to the backup root.
pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/backups/run", post(run_route))
        .with_state(store)
}

/// The staging directory, and the guard that removes it when the run
/// does not publish it.
struct Staging {
    path: PathBuf,
    published: bool,
}

impl Staging {
    fn create(root: &Path) -> Result<Self> {
        // The producer stages as `.staging.XXXXXX`; the name starts with
        // `.staging` so the producer's stale-directory sweep and the
        // gateway's own set reader (which only looks at timestamped
        // directories) both ignore it.
        for _ in 0..16 {
            let name = format!(".staging.{:016x}", rand::random::<u64>());
            let path = root.join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Staging {
                        path,
                        published: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(Error::internal()),
            }
        }
        Err(Error::internal())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published {
            // A failed run leaves nothing behind: not a half-written set,
            // not a staging directory for the next reader to trip over.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Capture every database and the registry into the staging directory.
fn capture(
    store: &Store,
    staging: &Staging,
    names: &[String],
    fault: FaultPoint,
) -> Result<(Vec<BackupRunDatabase>, BackupRunRegistry, u64)> {
    let mut databases = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let file = format!("{name}.sqlite");
        let path = staging.path.join(&file);
        // The store's own export: the SQLite online backup API, so the
        // copy is a consistent committed point in time even while the
        // database is being written. A raw file copy of a live WAL-mode
        // database is not a backup.
        store
            .export_to_limited(name, &path, Some(MAX_TRANSFER_SIZE))
            .map_err(|error| export_error(name, error))?;
        if fault == FaultPoint::CorruptDatabase && index == 0 {
            // Make a staged member something that is not a database. The
            // corruption happens BEFORE the size and the checksum are
            // taken, so the manifest agrees with the bytes on disk and
            // the SQLite check is what refuses the set.
            let bytes = std::fs::read(&path).unwrap_or_default();
            let rubbish = vec![b'x'; bytes.len().max(1)];
            write_file(&path, &rubbish)?;
        }
        let size = file_size(&path)?;
        let sha256 = sha256_file(&path)?;
        // The sidecar holds the sha256 and nothing else, exactly as the
        // producer writes it.
        write_file(
            &staging.path.join(format!("{name}.sha256")),
            format!("{sha256}\n").as_bytes(),
        )?;
        databases.push(BackupRunDatabase {
            name: name.clone(),
            file,
            size,
            sha256,
        });
    }
    if fault == FaultPoint::AfterDatabases {
        return Err(Error::new(
            500,
            "backup_fault_injected",
            "the run was failed on purpose after the databases were staged",
        ));
    }

    // The registry is copied with the same online backup API: the live
    // registry is a WAL-mode database, and a raw file copy of one is not
    // a valid backup.
    let registry_path = staging.path.join(REGISTRY_FILE);
    copy_registry(store, &registry_path)?;
    // The moment the registry copy was taken, taken the way the producer
    // takes it: right after the copy, not at the end of the run.
    let registry_captured_at = unix_secs(SystemTime::now());
    let registry = BackupRunRegistry {
        file: REGISTRY_FILE.to_string(),
        size: file_size(&registry_path)?,
        sha256: sha256_file(&registry_path)?,
    };
    if fault == FaultPoint::AfterRegistry {
        return Err(Error::new(
            500,
            "backup_fault_injected",
            "the run was failed on purpose after the registry was staged",
        ));
    }
    Ok((databases, registry, registry_captured_at))
}

/// A database that could not be captured, said in the operator's terms.
fn export_error(name: &str, error: Error) -> Error {
    if error.status == 413 {
        return Error::with_message(
            413,
            "result_limit_exceeded",
            format!(
                "the database {name} is larger than the {MAX_TRANSFER_SIZE} byte export ceiling, so it cannot be captured"
            ),
        );
    }
    Error::with_message(
        error.status,
        error.code,
        format!(
            "the database {name} could not be captured: {}",
            error.message
        ),
    )
}

/// The manifest, in the producer's shape.
///
/// The producer writes it with `indent=2, sort_keys=True` and a trailing
/// newline, so the same fields in the same order are a set the producer's
/// own reader accepts without complaint.
fn manifest_json(
    created: &str,
    timestamp: &str,
    started_at: u64,
    completed_at: u64,
    registry_captured_at: u64,
    databases: &[BackupRunDatabase],
    registry: &BackupRunRegistry,
) -> Value {
    let entries: Vec<Value> = databases
        .iter()
        .map(|database| {
            json!({
                "name": database.name,
                "file": database.file,
                "size": database.size,
                "sha256": database.sha256,
            })
        })
        .collect();
    json!({
        "created": created,
        "timestamp": timestamp,
        "started_at": started_at,
        "completed_at": completed_at,
        "registry_captured_at": registry_captured_at,
        "verification": VERIFICATION,
        "tool": TOOL,
        "dg_version": env!("CARGO_PKG_VERSION"),
        "databases": entries,
        "registry": {
            "file": registry.file,
            "size": registry.size,
            "sha256": registry.sha256,
        },
    })
}

/// Check the staged set the way the producer checks it, before it is
/// published.
///
/// Every member and the registry must be a standalone SQLite database
/// that passes `PRAGMA integrity_check`, and the manifest's sizes and
/// checksums must agree with the bytes on disk. The depth is the
/// gateway's own import depth (integrity, foreign keys, protected
/// tables), which is what `dg backup verify` applies to a published set:
/// a set this passes is a set the verifier accepts.
fn validate(staging: &Staging, manifest: &Value) -> Result<()> {
    for key in [
        "created",
        "timestamp",
        "started_at",
        "completed_at",
        "registry_captured_at",
        "verification",
        "tool",
        "dg_version",
        "databases",
        "registry",
    ] {
        if manifest.get(key).is_none() {
            return Err(Error::with_message(
                500,
                "backup_manifest_incomplete",
                format!("the staged manifest misses {key}"),
            ));
        }
    }
    let entries = manifest["databases"]
        .as_array()
        .ok_or_else(Error::internal)?;
    for entry in entries {
        let name = entry["name"].as_str().unwrap_or_default().to_string();
        let file = entry["file"].as_str().unwrap_or_default().to_string();
        let size = entry["size"].as_u64().unwrap_or(0);
        let sha256 = entry["sha256"].as_str().unwrap_or_default().to_string();
        if name.is_empty() || file.is_empty() || sha256.len() != 64 {
            return Err(Error::with_message(
                500,
                "backup_manifest_incomplete",
                "the staged manifest carries a malformed database entry".to_string(),
            ));
        }
        let path = staging.path.join(&file);
        check_member(&path, &name, size, &sha256)?;
        // The sidecar is part of the set's contract with the producer.
        let sidecar = std::fs::read_to_string(staging.path.join(format!("{name}.sha256")))
            .unwrap_or_default();
        if sidecar.trim() != sha256 {
            return Err(Error::with_message(
                500,
                "backup_validation_failed",
                format!("the staged checksum sidecar for {name} does not match the manifest"),
            ));
        }
    }
    let registry = &manifest["registry"];
    let file = registry["file"].as_str().unwrap_or_default().to_string();
    let size = registry["size"].as_u64().unwrap_or(0);
    let sha256 = registry["sha256"].as_str().unwrap_or_default().to_string();
    if file.is_empty() || sha256.len() != 64 {
        return Err(Error::with_message(
            500,
            "backup_manifest_incomplete",
            "the staged manifest carries a malformed registry entry".to_string(),
        ));
    }
    check_member(&staging.path.join(&file), "the registry", size, &sha256)?;
    Ok(())
}

/// One staged member: the bytes must agree with the manifest, and the
/// file must be a standalone SQLite database that passes its integrity
/// check.
fn check_member(path: &Path, label: &str, size: u64, sha256: &str) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|_| {
        Error::with_message(
            500,
            "backup_validation_failed",
            format!("the staged file for {label} is missing"),
        )
    })?;
    if metadata.len() != size {
        return Err(Error::with_message(
            500,
            "backup_validation_failed",
            format!("the staged file for {label} does not match the manifest's size"),
        ));
    }
    let actual = sha256_file(path)?;
    if actual != sha256 {
        return Err(Error::with_message(
            500,
            "backup_validation_failed",
            format!("the staged file for {label} does not match the manifest's checksum"),
        ));
    }
    let integrity: String = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|conn| {
            conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        })
        .map_err(|_| {
            Error::with_message(
                500,
                "backup_validation_failed",
                format!("the staged file for {label} is not a readable SQLite database"),
            )
        })?;
    if integrity != "ok" {
        return Err(Error::with_message(
            500,
            "backup_validation_failed",
            format!("the staged file for {label} failed integrity_check: {integrity}"),
        ));
    }
    // The gateway's own import depth: foreign keys and protected tables.
    // A set that passes this is a set `dg backup verify` accepts.
    crate::execution::validate_import(path).map_err(|error| {
        Error::with_message(
            500,
            "backup_validation_failed",
            format!(
                "the staged file for {label} failed validation: {}",
                error.message
            ),
        )
    })?;
    Ok(())
}

/// Copy the registry into the set, consistently.
///
/// The live registry is a WAL-mode database, so a raw file copy of it is
/// not a valid backup: the copy is taken with the SQLite online backup
/// API, exactly as the producer takes it.
fn copy_registry(store: &Store, destination: &Path) -> Result<()> {
    let source_path = store.root().join("metadata.sqlite");
    if !source_path.is_file() {
        return Err(Error::new(
            409,
            "backup_registry_missing",
            "the gateway's registry is missing, so no backup set can be published",
        ));
    }
    // Read-only first: the producer opens the registry read-only too. A
    // WAL-mode database can refuse a read-only open when its shared
    // index is not available, and the gateway owns this file, so the
    // fallback opens it the way the gateway itself does.
    let source = match Connection::open_with_flags(&source_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(_) => Connection::open(&source_path)?,
    };
    source.busy_timeout(Duration::from_millis(250))?;
    let mut target = Connection::open(destination)?;
    target.busy_timeout(Duration::from_millis(250))?;
    let backup = Backup::new(&source, &mut target)?;
    backup.run_to_completion(64, Duration::from_millis(0), None)?;
    drop(backup);
    // A rollback-journal destination is a standalone file: no sidecar is
    // needed to read it, and nothing is left in the set beside it.
    target.pragma_update(None, "journal_mode", "DELETE")?;
    let integrity: String =
        target.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
    if integrity != "ok" {
        return Err(Error::with_message(
            500,
            "backup_validation_failed",
            format!("the registry copy failed integrity_check: {integrity}"),
        ));
    }
    drop(target);
    for suffix in ["-wal", "-shm"] {
        let _ =
            std::fs::remove_file(destination.with_file_name(format!("{REGISTRY_FILE}{suffix}")));
    }
    Ok(())
}

/// The producer's lock, taken the producer's way: exclusive and
/// non-blocking. A refusal is a refusal, not a wait.
fn lock_backup_root(root: &Path) -> Result<File> {
    let path = root.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        // The lock file is shared with the producer: it is never
        // truncated, because the producer holds it open while it runs.
        .truncate(false)
        .open(&path)
        .map_err(|_| Error::internal())?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::new(
            409,
            "backup_locked",
            "another backup is already running",
        )),
        Err(std::fs::TryLockError::Error(_)) => Err(Error::internal()),
    }
}

/// The backup root, ready to receive a set.
fn prepare_root(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|_| Error::internal())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write a file's bytes.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|_| Error::internal())?;
    std::io::Write::write_all(&mut file, bytes).map_err(|_| Error::internal())?;
    file.sync_all().map_err(|_| Error::internal())?;
    Ok(())
}

/// A file's size.
fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .map_err(|_| Error::internal())?
        .len())
}

/// A file's sha256, streamed so a large database is never held in memory.
fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).map_err(|_| Error::internal())?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut chunk).map_err(|_| Error::internal())?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Flush a directory's own entries, so a name that is already on disk
/// survives a crash.
fn sync_directory(path: &Path) -> Result<()> {
    let directory = File::open(path).map_err(|_| Error::internal())?;
    directory.sync_all().map_err(|_| Error::internal())?;
    Ok(())
}

/// Flush every file in the staging directory, and the directory itself.
///
/// The producer does the same walk before it publishes: a set whose
/// bytes are not on disk is a set that does not exist.
fn sync_staging(staging: &Staging) -> Result<()> {
    let entries = std::fs::read_dir(&staging.path).map_err(|_| Error::internal())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| Error::internal())?;
        file.sync_all().map_err(|_| Error::internal())?;
    }
    sync_directory(&staging.path)
}

/// Seconds since the Unix epoch.
fn unix_secs(stamp: SystemTime) -> u64 {
    stamp
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// The UTC calendar date and time of a unix timestamp.
fn civil_from_epoch(epoch: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (epoch / 86_400) as i64;
    let seconds = epoch % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (seconds / 3_600) as u32,
        ((seconds % 3_600) / 60) as u32,
        (seconds % 60) as u32,
    )
}

/// Days since the epoch to a civil date.
///
/// The standard conversion: the era is 400 years long, the day of the
/// era is counted out of it, and the month is derived from a March-based
/// year so the leap day lands at the end.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The credential from the request.
///
/// This router is merged into the API's, and the API's helper is private
/// to that module, so the extraction is carried here.
fn credential(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(unauthorized)
}

/// Publish one backup set.
async fn run_route(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    let credential = credential(&headers)?;
    // The run holds ONE of the service's write permits for its whole
    // duration. The set spans every database, so a per-database lane
    // cannot express it: taking every database's lane in turn would
    // risk a deadlock against concurrent multi-database work and would
    // not be simpler. One permit of the service-wide lane is what
    // covers the whole deployment, and it makes the run count against
    // the write capacity the way any other writer does. The other
    // permits stay free, and reads keep flowing on the read lane.
    //
    // What the run does NOT do is freeze the service: each database's
    // copy is a consistent snapshot of that database, and the
    // databases in one set are not at the same instant -- exactly as in
    // the producer's sets, which export each database separately.
    let metrics = store.clone();
    metrics.metrics.record_request();
    let permit =
        match tokio::time::timeout(ACQUIRE_TIMEOUT, store.write_lane.clone().acquire_owned()).await
        {
            Ok(Ok(permit)) => permit,
            _ => {
                metrics.metrics.record_error("capacity_exceeded");
                return Err(Error::busy());
            }
        };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _lease = store.request_lease()?;
        let principal = store.authenticate(&credential)?;
        run(&store, &principal)
    })
    .await
    .map_err(|_| Error::internal())?;
    if let Err(error) = &result {
        metrics.metrics.record_error(error.code);
    }
    let outcome = result?;
    Ok(Json(
        serde_json::to_value(outcome).map_err(|_| Error::internal())?,
    ))
}
