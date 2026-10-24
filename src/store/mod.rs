//! Server metadata is separate from application SQL. Credentials are generated
//! locally by `dg init`/owner-only key issuance and only their SHA-256 hashes persist.
use crate::error::{Error, Result};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock, RwLockReadGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

mod admission;
mod audit;
mod auth;
mod backup_sets;
mod branches;
mod disk;
mod lifecycle;
mod metrics;
mod queries;
mod transfers;

pub use self::admission::{
    ACQUIRE_TIMEOUT, AdmissionGuard, AdmissionQueue, DbQueue, QueueGuard, READ_PERMITS,
    WRITE_PERMITS,
};
pub use self::audit::{AuditEntry, AuditLog};
pub use self::auth::{KeyRequest, Principal, hash, unauthorized};
pub use self::backup_sets::BackupSet;
pub(crate) use self::backup_sets::{is_set_dir, read_manifest_capped};
pub use self::branches::{BranchRecord, BranchRegistry};
pub use self::disk::{DiskSpace, disk_space};
pub use self::lifecycle::{Location, validate_name};
pub use self::metrics::{DbUsage, Metrics, MetricsSnapshot};
pub use self::queries::{
    MAX_PRINCIPALS_PER_SHAPE, MAX_SHAPES_PER_DATABASE, QueryLog, QueryShape, StatementRun,
};
pub use self::transfers::{
    MAX_CHUNK, MAX_TRANSFER_SIZE, MAX_TRANSFERS, SnapshotInfo, Staging, TRANSFER_TTL, UploadInfo,
};

// Cross-module internals: sibling submodules share these through the
// store namespace (each is `pub(super)` in its home module, so nothing
// here widens the crate-visible surface).
#[cfg(test)]
use self::auth::map_issue_error;
use self::auth::{generate_key, migrate_legacy_scopes, new_generation};
use self::backup_sets::{default_backup_root, load_write_markers_from_connection};
use self::lifecycle::{migrate_metadata, remove_database_files};
use self::transfers::{Transfer, remove_staging_dir, sweep_orphan_transfer_dirs};
#[cfg(test)]
use self::transfers::{TransferKind, UploadState};

/// Maximum registered databases (live rows plus unpurged tombstones).
/// Sized for the hundreds-of-databases requirement with headroom: registry
/// rows are cheap and per-database runtime state is created lazily, so this
/// bound exists to keep a runaway client from filling the disk with database
/// files, not because the registry strains near it.
pub const MAX_DATABASES: i64 = 1024;

pub struct Store {
    root: PathBuf,
    pub read_lane: Arc<Semaphore>,
    pub write_lane: Arc<Semaphore>,
    // Per-database guards: std RwLock below is the lifecycle lease, while
    // these tokio RwLocks are admission lanes — fully qualified to keep the
    // two apart.
    queues: Mutex<HashMap<String, Arc<AdmissionQueue>>>,
    pub metrics: Metrics,
    /// Last successful API write per database, loaded from metadata on
    /// startup so freshness cannot flip to false fresh across a restart.
    writes: Mutex<HashMap<String, SystemTime>>,
    /// Serializes durable freshness-marker updates. A negative marker is
    /// written before each mutation and replaced only after commit.
    marker_lock: Mutex<()>,
    /// A long-lived metadata connection avoids opening and reconfiguring a
    /// SQLite connection for every write marker. `marker_lock` serializes
    /// callers; this mutex also keeps the connection itself single-owner.
    marker_db: Mutex<Connection>,
    /// Versioned backup sets published by the external backup job
    /// (scripts/micro2-backup.sh layout). Convention: `<data-dir>/../dg-backups`
    /// unless overridden; absent/unreadable means backup status is unknown,
    /// never healthy.
    backup_root: Option<PathBuf>,
    // All server requests hold a read lease until their connections close.
    // Purge takes an exclusive lease before unlinking SQLite files.
    lifecycle: RwLock<()>,
    transfers: Mutex<HashMap<String, Transfer>>,
    transfer_ttl: Duration,
    /// Which databases are forks of which, and when the fork was taken.
    pub branches: BranchRegistry,
    /// Which credential ran what, and when.
    pub audit: AuditLog,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_ttl(root, TRANSFER_TTL)
    }

    /// Open the store WITHOUT the owner-only startup work.
    ///
    /// A second process opens the same data directory the live gateway
    /// owns: the shipped `dg backup digest` unit runs on the host where
    /// `dg.service` runs continuously. The owner-only work must not run
    /// there. The orphan sweep deleted the `.transfer-*` and `.lifecycle-*`
    /// directories the live gateway was still using for an in-flight
    /// chunked export or import, so an export that overlapped the daily
    /// digest failed with an unexplained 500, and the registry migration
    /// took the metadata write lock from under the running gateway. A
    /// reader repairs nothing: it reports, and it leaves the owner's
    /// in-flight work alone.
    pub fn open_shared(root: &Path) -> Result<Self> {
        Self::open_impl(root, TRANSFER_TTL, false)
    }

    /// Override the backup-set root (serve `--backup-root`). `None` disables
    /// backup visibility: `/v1/backups` reports unknown, never healthy.
    pub fn with_backup_root(mut self, root: Option<PathBuf>) -> Self {
        self.backup_root = root;
        self
    }

    pub fn open_with_ttl(root: &Path, transfer_ttl: Duration) -> Result<Self> {
        Self::open_impl(root, transfer_ttl, true)
    }

    /// `owner` decides whether the caller is the process that owns the
    /// data directory. The owner performs the startup work a fresh
    /// directory needs (and a crashed predecessor left behind): sweeping
    /// orphaned staging directories and migrating the registry. A reader
    /// performs neither — see [`Store::open_shared`].
    fn open_impl(root: &Path, transfer_ttl: Duration, owner: bool) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(|_| Error::internal())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::internal())?;
        }
        if owner {
            sweep_orphan_transfer_dirs(root);
        }
        let backup_root = default_backup_root(root);
        let mut metadata = Connection::open(root.join("metadata.sqlite"))?;
        metadata.busy_timeout(Duration::from_millis(250))?;
        metadata.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA cache_size=-1024;",
        )?;
        if owner {
            migrate_metadata(&mut metadata, root)?;
        }
        let writes = load_write_markers_from_connection(&metadata).unwrap_or_default();
        let store = Self {
            root: root.to_path_buf(),
            read_lane: Arc::new(Semaphore::new(READ_PERMITS)),
            write_lane: Arc::new(Semaphore::new(WRITE_PERMITS)),
            queues: Mutex::new(HashMap::new()),
            metrics: Metrics::new(),
            writes: Mutex::new(writes),
            marker_lock: Mutex::new(()),
            marker_db: Mutex::new(metadata),
            backup_root,
            lifecycle: RwLock::new(()),
            transfers: Mutex::new(HashMap::new()),
            transfer_ttl,
            branches: BranchRegistry::default(),
            audit: AuditLog::default(),
        };
        Ok(store)
    }

    /// The directory the gateway keeps its databases in.
    ///
    /// This is the path the gateway was started with, not a resolved one:
    /// it is the path the operator put in the unit file, so it is the
    /// path they can paste into another tool.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn meta(&self) -> Result<Connection> {
        let conn = Connection::open(self.root.join("metadata.sqlite"))?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-1024;")?;
        Ok(conn)
    }
}

#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod purge_tests;
#[cfg(test)]
mod tests;
