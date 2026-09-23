//! Server metadata is separate from application SQL. Credentials are generated
//! locally by `dg init`/owner-only key issuance and only their SHA-256 hashes persist.
use crate::error::{Error, Result};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, RwLockReadGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

/// Maximum registered databases (live rows plus unpurged tombstones).
/// Sized for the hundreds-of-databases requirement with headroom: registry
/// rows are cheap and per-database runtime state is created lazily, so this
/// bound exists to keep a runaway client from filling the disk with database
/// files, not because the registry strains near it.
pub const MAX_DATABASES: i64 = 1024;

/// Bounded admission wait (scale proposal Phase 1). Bursty arrivals queue up
/// to this long for a permit instead of failing instantly; overload still
/// surfaces as the same `429 capacity_exceeded`, only after waiting.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(500);
/// Admission lane sizes (scale proposal Phase 2): reads share a wide lane
/// and a shared per-database read guard; writes take the narrow lane and an
/// exclusive per-database write guard. Starting point from the F2b
/// workloads; re-derive from measurement before changing (Phase 3).
pub const READ_PERMITS: usize = 8;
pub const WRITE_PERMITS: usize = 4;

/// Held per-database admission guard. Readers share; writers exclude.
/// Dropped at the end of the request to release the guard.
pub enum QueueGuard {
    Read(tokio::sync::OwnedRwLockReadGuard<()>),
    Write(tokio::sync::OwnedRwLockWriteGuard<()>),
}

/// Request-pressure counters (observability contract §1). Status-level only:
/// total plus the three shed/degrade signals. Updated at the API layer in
/// `work_in`, which every route funnels through; per-code detail beyond
/// these signals stays in the structured logs, not in memory.
pub struct Metrics {
    started: Instant,
    total: AtomicU64,
    capacity_exceeded: AtomicU64,
    deadline_exceeded: AtomicU64,
    internal_error: AtomicU64,
}

/// Point-in-time copy for `/v1/status`. Plain data, no locks held.
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub total: u64,
    pub capacity_exceeded: u64,
    pub deadline_exceeded: u64,
    pub internal_error: u64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            total: AtomicU64::new(0),
            capacity_exceeded: AtomicU64::new(0),
            deadline_exceeded: AtomicU64::new(0),
            internal_error: AtomicU64::new(0),
        }
    }
    pub fn record_request(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_error(&self, code: &str) {
        let counter = match code {
            "capacity_exceeded" => &self.capacity_exceeded,
            "query_deadline_exceeded" => &self.deadline_exceeded,
            "internal_error" => &self.internal_error,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> MetricsSnapshot {
        let load = Ordering::Relaxed;
        MetricsSnapshot {
            uptime_secs: self.started.elapsed().as_secs(),
            total: self.total.load(load),
            capacity_exceeded: self.capacity_exceeded.load(load),
            deadline_exceeded: self.deadline_exceeded.load(load),
            internal_error: self.internal_error.load(load),
        }
    }
}

pub struct Store {
    root: PathBuf,
    pub read_lane: Arc<Semaphore>,
    pub write_lane: Arc<Semaphore>,
    // Per-database guards: std RwLock below is the lifecycle lease, while
    // these tokio RwLocks are admission lanes — fully qualified to keep the
    // two apart.
    queues: Mutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
    pub metrics: Metrics,
    /// Last successful API write per database, loaded from metadata on
    /// startup so freshness cannot flip to false fresh across a restart.
    writes: Mutex<HashMap<String, SystemTime>>,
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
}

/// Chunked-transfer bounds (large-database export/import). Chunks keep every
/// request and response small; the total ceiling is the documented maximum
/// database size for lifecycle transfer.
pub const MAX_TRANSFER_SIZE: u64 = 128 * 1024 * 1024;
pub const MAX_CHUNK: usize = 1024 * 1024;
pub const MAX_TRANSFERS: usize = 8;
pub const TRANSFER_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
enum TransferKind {
    Snapshot { size: u64, sha256: String },
    Upload { received: u64, state: UploadState },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UploadState {
    Open,
    Committing,
}

#[derive(Clone)]
struct Transfer {
    kind: TransferKind,
    dir: PathBuf,
    file: PathBuf,
    /// Creator's credential hash. Snapshot chunks/release require the creator
    /// or the owner; uploads are owner-only at the API layer.
    creator: String,
    created: Instant,
}

pub struct SnapshotInfo {
    pub id: String,
    pub size: u64,
    pub sha256: String,
}

pub struct UploadInfo {
    pub id: String,
}

/// An operation exclusively owns this directory and all SQLite sidecars in it.
/// Drop cleans up on every success/error path, without touching another request.
pub struct Staging {
    _directory: tempfile::TempDir,
    path: PathBuf,
}
impl Staging {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DatabaseScope {
    name: String,
    generation: String,
}

#[derive(Clone)]
pub struct Principal {
    pub hash: String,
    pub role: String,
    /// Names are retained for the public inventory shape; authorization uses
    /// the private generation-bound scopes below.
    pub databases: Vec<String>,
    scopes: Vec<DatabaseScope>,
}
impl Principal {
    pub fn owner(&self) -> Result<()> {
        if self.role == "owner" {
            Ok(())
        } else {
            Err(Error::new(403, "forbidden", "owner required"))
        }
    }
    fn database(&self, name: &str, generation: Option<&str>) -> Result<()> {
        let allowed = self.role == "owner"
            || self.scopes.iter().any(|scope| {
                scope.name == name
                    && generation
                        .map(|current| scope.generation == current)
                        .unwrap_or(true)
            });
        if allowed {
            Ok(())
        } else {
            Err(Error::new(
                403,
                "database_forbidden",
                "database access denied",
            ))
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRequest {
    pub name: String,
    pub role: String,
    pub databases: Vec<String>,
}
pub fn hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}
fn generate_key() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || name.starts_with('_')
    {
        return Err(Error::invalid());
    }
    Ok(())
}
/// Backup-set convention: versioned sets live next to the data dir unless
/// `--backup-root` overrides. Returns None for degenerate data-dir paths.
fn default_backup_root(data_dir: &Path) -> Option<PathBuf> {
    let parent = data_dir.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.join("dg-backups"))
}

/// Versioned-set directory names look like `20260922T223715Z` (the backup
/// script's `%Y%m%dT%H%M%SZ`). Staging dirs and anything else never match.
fn is_set_dir(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 16
        && b[..8].iter().all(|c| c.is_ascii_digit())
        && b[8] == b'T'
        && b[9..15].iter().all(|c| c.is_ascii_digit())
        && b[15] == b'Z'
}

/// One published versioned set (scripts/micro2-backup.sh layout): directory
/// name, filesystem mtime, member database names, registry presence.
pub struct BackupSet {
    pub timestamp: String,
    pub mtime: SystemTime,
    pub databases: Vec<String>,
    pub has_registry: bool,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_ttl(root, TRANSFER_TTL)
    }

    /// Override the backup-set root (serve `--backup-root`). `None` disables
    /// backup visibility: `/v1/backups` reports unknown, never healthy.
    pub fn with_backup_root(mut self, root: Option<PathBuf>) -> Self {
        self.backup_root = root;
        self
    }

    /// Record a successful API mutation. The durable marker is safety state;
    /// persistence failure leaves freshness conservative after restart.
    pub fn note_write(&self, name: &str) {
        let stamp = SystemTime::now();
        if let Err(error) = self.persist_write(name, stamp) {
            tracing::warn!(database = name, error = %error, "backup write marker not persisted");
        }
        if let Ok(mut writes) = self.writes.lock() {
            writes.insert(name.to_string(), stamp);
        }
    }

    pub fn last_write(&self, name: &str) -> Option<SystemTime> {
        self.writes
            .lock()
            .ok()
            .and_then(|writes| writes.get(name).copied())
    }

    fn persist_write(&self, name: &str, stamp: SystemTime) -> Result<()> {
        let seconds = stamp
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::internal())?
            .as_secs() as i64;
        self.meta()?
            .execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, seconds],
            )
            .map_err(|_| Error::internal())?;
        Ok(())
    }

    fn load_write_markers(&self) -> HashMap<String, SystemTime> {
        let Ok(conn) = self.meta() else {
            return HashMap::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT name,written_at FROM write_markers") else {
            return HashMap::new();
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        rows.into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .filter_map(|(name, seconds)| {
                let seconds = u64::try_from(seconds).ok()?;
                UNIX_EPOCH
                    .checked_add(Duration::from_secs(seconds))
                    .map(|stamp| (name, stamp))
            })
            .collect()
    }

    pub fn backup_root(&self) -> Option<PathBuf> {
        self.backup_root.clone()
    }

    /// Versioned sets newest-first. None when no root is configured or the
    /// root is unreadable; unreadable individual sets are skipped, never
    /// presented as healthy.
    pub fn backup_sets(&self) -> Option<Vec<BackupSet>> {
        let root = self.backup_root.as_ref()?;
        let entries = std::fs::read_dir(root).ok()?;
        let mut sets = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_set_dir(&name) || !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(entry.path().join("MANIFEST.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let databases = manifest["databases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let has_registry = manifest["registry"]["sha256"].as_str().is_some();
            sets.push(BackupSet {
                timestamp: name,
                mtime,
                databases,
                has_registry,
            });
        }
        sets.sort_by_key(|set| std::cmp::Reverse(set.mtime));
        Some(sets)
    }

    pub fn open_with_ttl(root: &Path, transfer_ttl: Duration) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(|_| Error::internal())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::internal())?;
        }
        sweep_orphan_transfer_dirs(root);
        let backup_root = default_backup_root(root);
        let store = Self {
            root: root.to_path_buf(),
            read_lane: Arc::new(Semaphore::new(READ_PERMITS)),
            write_lane: Arc::new(Semaphore::new(WRITE_PERMITS)),
            queues: Mutex::new(HashMap::new()),
            metrics: Metrics::new(),
            writes: Mutex::new(HashMap::new()),
            backup_root,
            lifecycle: RwLock::new(()),
            transfers: Mutex::new(HashMap::new()),
            transfer_ttl,
        };
        let mut metadata = store.meta()?;
        metadata.execute_batch("PRAGMA journal_mode=WAL;")?;
        migrate_metadata(&mut metadata, &store.root)?;
        let writes = store.load_write_markers();
        *store.writes.lock().map_err(|_| Error::internal())? = writes;
        Ok(store)
    }
    fn meta(&self) -> Result<Connection> {
        let conn = Connection::open(self.root.join("metadata.sqlite"))?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-1024;")?;
        Ok(conn)
    }
    pub fn bootstrap(&self) -> Result<String> {
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT count(*) FROM credentials WHERE role='owner'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0
        {
            return Err(Error::new(
                409,
                "already_bootstrapped",
                "owner already exists",
            ));
        }
        let key = generate_key();
        tx.execute(
            "INSERT INTO credentials VALUES ('owner',?,'owner','[]')",
            [hash(&key)],
        )?;
        tx.commit()?;
        Ok(key)
    }
    pub fn authenticate(&self, key: &str) -> Result<Principal> {
        if key.len() != 64 {
            return Err(unauthorized());
        }
        let hash = hash(key);
        let result = self.meta()?.query_row(
            "SELECT role,databases FROM credentials WHERE hash=?",
            [&hash],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        );
        match result {
            Ok((role, dbs)) => {
                let scopes = parse_scopes(&dbs)?;
                Ok(Principal {
                    hash,
                    role,
                    databases: scope_names(&scopes),
                    scopes,
                })
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(unauthorized()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn issue(&self, request: KeyRequest) -> Result<String> {
        validate_name(&request.name)?;
        if request.name == "owner"
            || !matches!(
                request.role.as_str(),
                "database_read_only" | "database_read_write"
            )
            || request.databases.is_empty()
            || request.databases.len() > 32
        {
            return Err(Error::invalid());
        }
        for name in &request.databases {
            validate_name(name)?;
        }
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM credentials", [], |r| {
            r.get::<_, i64>(0)
        })? >= 256
        {
            return Err(Error::busy());
        }
        let mut scopes = Vec::with_capacity(request.databases.len());
        for name in &request.databases {
            let generation: Option<String> = tx
                .query_row(
                    "SELECT generation FROM databases WHERE name=? AND deleted_at IS NULL",
                    [name],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(generation) = generation else {
                return Err(Error::new(404, "database_not_found", "database not found"));
            };
            if generation.is_empty() {
                return Err(Error::internal());
            }
            scopes.push(DatabaseScope {
                name: name.clone(),
                generation,
            });
        }
        let key = generate_key();
        tx.execute(
            "INSERT INTO credentials VALUES (?,?,?,?)",
            params![
                request.name,
                hash(&key),
                request.role,
                serde_json::to_string(&scopes).map_err(|_| Error::internal())?
            ],
        )
        .map_err(|_| Error::new(409, "key_exists", "credential name already exists"))?;
        tx.commit()?;
        Ok(key)
    }
    /// Owner-visible credential inventory: name, role, and scope.
    /// Hashes never leave the server; the owner row itself is listed.
    pub fn list_keys(&self) -> Result<serde_json::Value> {
        let conn = self.meta()?;
        let mut stmt =
            conn.prepare("SELECT name, role, databases FROM credentials ORDER BY name")?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut keys = Vec::with_capacity(rows.len());
        for (name, role, databases) in rows {
            let scopes = parse_scopes(&databases)?;
            keys.push(serde_json::json!({
                "name": name,
                "role": role,
                "databases": scope_names(&scopes),
            }));
        }
        Ok(serde_json::json!({"keys": keys}))
    }
    pub fn revoke(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if name == "owner" {
            return Err(Error::invalid());
        }
        self.meta()?.execute(
            "DELETE FROM credentials WHERE name=? AND role != 'owner'",
            [name],
        )?;
        Ok(())
    }

    /// Replace the owner credential atomically and return the new raw key
    /// (shown once). The old key stops authenticating immediately; database
    /// rows, generations, and app-key scopes are untouched. Only reachable
    /// with a valid owner key, so a missing owner row is corruption, not a
    /// client error.
    pub fn rotate_owner(&self) -> Result<String> {
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let key = generate_key();
        let changed = tx.execute(
            "UPDATE credentials SET hash=? WHERE name='owner' AND role='owner'",
            [hash(&key)],
        )?;
        if changed != 1 {
            return Err(Error::internal());
        }
        tx.commit()?;
        Ok(key)
    }
    /// Authorize a principal against the database's current generation.
    /// Missing rows are left to `path()` so a scoped credential retains the
    /// existing 404 behavior for a purged database, while a replacement with
    /// a different generation is denied before SQLite is opened.
    pub fn authorize(&self, principal: &Principal, name: &str) -> Result<()> {
        if principal.role == "owner" {
            return Ok(());
        }
        let generation = self.generation_for(name)?;
        principal.database(name, generation.as_deref())
    }

    fn generation_for(&self, name: &str) -> Result<Option<String>> {
        validate_name(name)?;
        let row: Option<Option<String>> = self
            .meta()?
            .query_row(
                "SELECT generation FROM databases WHERE name=?",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(Some(generation)) if !generation.is_empty() => Ok(Some(generation)),
            Some(Some(_)) | Some(None) => Err(Error::internal()),
            None => Ok(None),
        }
    }

    pub fn list(&self, principal: &Principal) -> Result<Vec<String>> {
        let conn = self.meta()?;
        let mut stmt = conn.prepare(
            "SELECT name,generation FROM databases WHERE deleted_at IS NULL ORDER BY name",
        )?;
        let names = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names
            .into_iter()
            .filter(|(name, generation)| principal.database(name, Some(generation)).is_ok())
            .map(|(name, _)| name)
            .collect())
    }
    /// Owner-visible database facts: size, user tables with row counts,
    /// creation time, and tombstone state. Tombstoned names report their
    /// retention deadline instead of table detail; unknown names are 404.
    pub fn info(&self, name: &str) -> Result<serde_json::Value> {
        validate_name(name)?;
        let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = self
            .meta()?
            .query_row(
                "SELECT deleted_at, purge_after, created_at FROM databases WHERE name=?",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some((deleted_at, purge_after, created_at)) = row else {
            return Err(Error::new(404, "database_not_found", "database not found"));
        };
        let path = self.root.join(format!("{name}.sqlite"));
        let size_bytes = database_size_bytes(&path);
        if deleted_at.is_some() {
            return Ok(serde_json::json!({
                "name": name,
                "tombstoned": true,
                "purge_after": purge_after,
                "size_bytes": size_bytes,
            }));
        }
        let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| Error::internal())?;
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))!='_dg_' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut detail = Vec::with_capacity(tables.len());
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            let rows: i64 =
                db.query_row(&format!("SELECT count(*) FROM \"{quoted}\""), [], |r| {
                    r.get(0)
                })?;
            detail.push(serde_json::json!({"name": table, "rows": rows}));
        }
        Ok(serde_json::json!({
            "name": name,
            "tombstoned": false,
            "size_bytes": size_bytes,
            "created_at": created_at,
            "tables": detail,
        }))
    }
    pub fn create(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM databases", [], |r| r.get::<_, i64>(0))?
            >= MAX_DATABASES
        {
            return Err(Error::busy());
        }
        let generation = new_generation(&tx)?;
        let path = self.root.join(format!("{name}.sqlite"));
        // create_new means an orphan after a crash is never overwritten. An
        // unregistered file needs operator recovery, not silent adoption.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| {
                Error::new(
                    409,
                    "database_exists",
                    "database file exists or cannot be created",
                )
            })?;
        drop(file);
        let result = (|| {
            crate::execution::initialize(&path)?;
            tx.execute(
                "INSERT INTO databases (name, created_at, generation) VALUES (?, ?, ?)",
                rusqlite::params![name, now_secs()?, generation],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            // Initialization and registry publication are one logical create:
            // never leave a file behind when either pre-commit step fails.
            remove_database_files(&path);
        }
        result
    }
    pub fn path(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        let found: bool = self.meta()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE name=? AND deleted_at IS NULL)",
            [name],
            |r| r.get(0),
        )?;
        if !found {
            return Err(Error::new(404, "database_not_found", "database not found"));
        }
        let path = self.root.join(format!("{name}.sqlite"));
        if !path.is_file() {
            return Err(Error::internal());
        }
        Ok(path)
    }
    pub fn queue(&self, name: &str) -> Result<Arc<tokio::sync::RwLock<()>>> {
        // Call only after path() verifies a bounded registry entry.
        let mut queues = self.queues.lock().map_err(|_| Error::internal())?;
        Ok(queues
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone())
    }

    /// Per-database admission with bounded waiting. Readers take the shared
    /// guard (WAL allows concurrent readers against one writer); writers
    /// take the exclusive guard. Runs on a blocking thread (it cannot await,
    /// so it polls with short sleeps); call only after path() verifies a
    /// bounded registry entry, same as queue(). Polling is not FIFO-fair
    /// like the async global permits, and a continuous read flood could
    /// delay a writer — waits stay capped at ACQUIRE_TIMEOUT, and a
    /// writer-preferring bound gets added only if measurement shows
    /// starvation (scale proposal Phase 2). Returns the guard to hold.
    pub fn acquire_queue(&self, name: &str, write: bool) -> Result<QueueGuard> {
        let queue = self.queue(name)?;
        let start = Instant::now();
        loop {
            if write {
                if let Ok(guard) = queue.clone().try_write_owned() {
                    return Ok(QueueGuard::Write(guard));
                }
            } else if let Ok(guard) = queue.clone().try_read_owned() {
                return Ok(QueueGuard::Read(guard));
            }
            if start.elapsed() >= ACQUIRE_TIMEOUT {
                return Err(Error::busy());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// SQLite online backup of `name` to `dest`, consistent during writes.
    /// Pages are copied in bounded steps so concurrent commits interleave;
    /// the snapshot reflects one committed point in time.
    pub fn export_to(&self, name: &str, dest: &Path) -> Result<()> {
        self.export_to_limited(name, dest, None)
    }

    /// SQLite online backup with an optional hard output ceiling. The check
    /// runs during backup steps so an over-limit snapshot is not fully
    /// materialized before the caller receives its 413.
    pub fn export_to_limited(&self, name: &str, dest: &Path, max_bytes: Option<u64>) -> Result<()> {
        let src_path = self.path(name)?;
        if dest.exists() {
            return Err(Error::new(
                409,
                "file_exists",
                "destination file already exists",
            ));
        }
        // Deliberately no per-database write-slot acquisition: the SQLite
        // online backup API on a WAL source produces a consistent committed
        // snapshot while writes continue (the copy restarts if the source
        // changes between steps). Bounding the step count keeps a
        // pathological concurrent-writer restart loop finite.
        let src =
            Connection::open_with_flags(&src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        src.busy_timeout(Duration::from_millis(250))?;
        let mut dst = Connection::open(dest)?;
        dst.busy_timeout(Duration::from_millis(250))?;
        let backup = Backup::new(&src, &mut dst)?;
        let mut result = StepResult::More;
        let mut steps = 0;
        while result == StepResult::More {
            steps += 1;
            if steps > 512 {
                result = StepResult::Busy;
                break;
            }
            result = backup.step(64).map_err(crate::error::Error::from)?;
            if max_bytes.is_some_and(|limit| {
                std::fs::metadata(dest)
                    .map(|metadata| metadata.len() > limit)
                    .unwrap_or(false)
            }) {
                drop(backup);
                drop(dst);
                remove_database_files(dest);
                return Err(Error::new(
                    413,
                    "result_limit_exceeded",
                    "export exceeds bounded response size",
                ));
            }
        }
        drop(backup);
        match result {
            StepResult::Done => {
                dst.pragma_update(None, "journal_mode", "DELETE")?;
                Ok(())
            }
            _ => {
                drop(dst);
                remove_database_files(dest);
                Err(Error::new(
                    429,
                    "capacity_exceeded",
                    "backup could not complete; source busy",
                ))
            }
        }
    }

    pub fn request_lease(&self) -> Result<RwLockReadGuard<'_, ()>> {
        self.lifecycle.read().map_err(|_| Error::internal())
    }

    fn staging(&self) -> Result<Staging> {
        let directory = tempfile::Builder::new()
            .prefix(".lifecycle-")
            .tempdir_in(&self.root)
            .map_err(|_| Error::internal())?;
        let path = directory.path().join("snapshot.sqlite");
        Ok(Staging {
            _directory: directory,
            path,
        })
    }

    /// Snapshot in a private directory, automatically removed by its owner.
    pub fn export_snapshot(&self, name: &str) -> Result<Staging> {
        self.export_snapshot_limited(name, None)
    }

    pub fn export_snapshot_limited(&self, name: &str, max_bytes: Option<u64>) -> Result<Staging> {
        let temp = self.staging()?;
        self.export_to_limited(name, temp.path(), max_bytes)?;
        Ok(temp)
    }

    pub fn import_staging(&self, name: &str) -> Result<Staging> {
        validate_name(name)?;
        self.staging()
    }

    /// Tombstone `name`: registry row keeps the file under lock so the
    /// name can neither be queried nor recreated. Physical purge is delayed
    /// `retention_secs` and recorded via `purge_after`.
    pub fn delete(&self, name: &str, retention_secs: i64) -> Result<()> {
        validate_name(name)?;
        let now = now_secs()?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE databases SET deleted_at=?2, purge_after=?3
             WHERE name=?1 AND deleted_at IS NULL",
            params![name, now, now + retention_secs],
        )?;
        if changed == 0 {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM databases WHERE name=?)",
                [name],
                |r| r.get(0),
            )?;
            return Err(if exists {
                Error::new(409, "database_exists", "database already deleted")
            } else {
                Error::new(404, "database_not_found", "database not found")
            });
        }
        tx.commit()?;
        Ok(())
    }

    /// Physically remove files for tombstones whose retention elapsed.
    /// Idempotent, driven by the server maintenance loop even when idle.
    /// Skip a busy tick; never unlink while any request owns a connection.
    pub fn purge_expired(&self) -> Result<Vec<String>> {
        let _exclusive = match self.lifecycle.try_write() {
            Ok(lease) => lease,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(Vec::new()),
            Err(_) => return Err(Error::internal()),
        };
        let now = now_secs()?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let due: Vec<String> = tx
            .prepare("SELECT name FROM databases WHERE deleted_at IS NOT NULL AND purge_after<=?")?
            .query_map([now], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut purged = Vec::new();
        for name in due {
            let base = self.root.join(format!("{name}.sqlite"));
            for suffix in ["", "-wal", "-shm"] {
                let path = path_with_suffix(&base, suffix);
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|_| Error::internal())?;
                }
            }
            tx.execute(
                "DELETE FROM databases WHERE name=? AND deleted_at IS NOT NULL",
                [&name],
            )?;
            purged.push(name);
        }
        tx.commit()?;
        Ok(purged)
    }

    /// Validate `source` and publish it as the new database `name` in one
    /// atomic registry step; a failed import leaves no partial database.
    pub fn import_from(&self, name: &str, source: &Path) -> Result<()> {
        validate_name(name)?;
        // Copy, validate, and initialize without holding the metadata write
        // lock. The registry transaction below is only the short publication
        // step, so a large independent import cannot stall authentication and
        // all other registry operations.
        let staged = self.staging()?;
        std::fs::copy(source, staged.path()).map_err(|_| Error::invalid())?;
        crate::execution::validate_import(staged.path())?;
        crate::execution::initialize(staged.path())?;

        let path = self.root.join(format!("{name}.sqlite"));
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM databases", [], |r| r.get::<_, i64>(0))?
            >= MAX_DATABASES
        {
            return Err(Error::busy());
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE name=?)",
            [name],
            |r| r.get(0),
        )?;
        if exists {
            // Covers both live databases and tombstones: importing over a
            // tombstoned row would silently overwrite the retained file.
            return Err(Error::new(409, "database_exists", "database exists"));
        }
        let generation = new_generation(&tx)?;
        std::fs::hard_link(staged.path(), &path).map_err(|_| {
            Error::new(
                409,
                "database_exists",
                "database file exists or cannot be published",
            )
        })?;
        let result = (|| {
            tx.execute(
                "INSERT INTO databases (name, created_at, generation) VALUES (?, ?, ?)",
                rusqlite::params![name, now_secs()?, generation],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            remove_database_files(&path);
        }
        result
    }

    /// Clone `source` into new database `dest` via the online backup API;
    /// the copy is validated and registered atomically by import_from.
    pub fn clone_database(&self, source: &str, dest: &str) -> Result<()> {
        validate_name(dest)?;
        let temp = self.export_snapshot(source)?;
        self.import_from(dest, temp.path())
    }

    /// Take a retained online-backup snapshot for chunked download. The
    /// snapshot is a consistent committed point in time; chunks served later
    /// read this frozen copy, so concurrent writes never tear a transfer.
    pub fn export_begin(&self, principal: &Principal, name: &str) -> Result<SnapshotInfo> {
        let dir = self.retained_transfer_dir()?;
        let file = dir.join("snapshot.sqlite");
        let outcome = (|| {
            self.export_to_limited(name, &file, Some(MAX_TRANSFER_SIZE))?;
            let size = std::fs::metadata(&file)
                .map_err(|_| Error::internal())?
                .len();
            if size > MAX_TRANSFER_SIZE {
                return Err(Error::new(
                    413,
                    "result_limit_exceeded",
                    "database exceeds chunked transfer ceiling",
                ));
            }
            let sha256 = sha256_file(&file)?;
            let id = generate_key();
            self.insert_transfer(
                id.clone(),
                Transfer {
                    kind: TransferKind::Snapshot {
                        size,
                        sha256: sha256.clone(),
                    },
                    dir: dir.clone(),
                    file,
                    creator: principal.hash.clone(),
                    created: Instant::now(),
                },
            )?;
            Ok(SnapshotInfo { id, size, sha256 })
        })();
        if outcome.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        outcome
    }

    /// Read one chunk of a snapshot. Unknown, expired, wrong-kind, and
    /// foreign-principal ids all report 404 so ids are not an oracle.
    pub fn snapshot_chunk(
        &self,
        principal: &Principal,
        id: &str,
        offset: u64,
        length: usize,
    ) -> Result<(Vec<u8>, u64, String)> {
        let transfer = self.lookup_transfer(id)?.ok_or_else(snapshot_missing)?;
        if transfer.creator != principal.hash && principal.role != "owner" {
            return Err(snapshot_missing());
        }
        let (size, sha256) = match &transfer.kind {
            TransferKind::Snapshot { size, sha256 } => (*size, sha256.clone()),
            TransferKind::Upload { .. } => return Err(snapshot_missing()),
        };
        if offset > size {
            return Err(Error::invalid());
        }
        let take = (length as u64).min(MAX_CHUNK as u64).min(size - offset) as usize;
        let mut file = std::fs::File::open(&transfer.file).map_err(|_| Error::internal())?;
        use std::io::{Read, Seek};
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(|_| Error::internal())?;
        let mut bytes = vec![0u8; take];
        file.read_exact(&mut bytes).map_err(|_| Error::internal())?;
        Ok((bytes, size, sha256))
    }

    pub fn snapshot_release(&self, principal: &Principal, id: &str) -> Result<()> {
        let transfer = self.lookup_transfer(id)?.ok_or_else(snapshot_missing)?;
        if transfer.creator != principal.hash && principal.role != "owner" {
            return Err(snapshot_missing());
        }
        if !matches!(transfer.kind, TransferKind::Snapshot { .. }) {
            return Err(snapshot_missing());
        }
        self.remove_transfer(id);
        Ok(())
    }

    /// Open a retained upload for chunked import. Owner-only at the API layer.
    pub fn upload_begin(&self) -> Result<UploadInfo> {
        let dir = self.retained_transfer_dir()?;
        let file = dir.join("upload.bin");
        std::fs::write(&file, []).map_err(|_| Error::internal())?;
        let id = generate_key();
        let outcome = self.insert_transfer(
            id.clone(),
            Transfer {
                kind: TransferKind::Upload {
                    received: 0,
                    state: UploadState::Open,
                },
                dir: dir.clone(),
                file,
                creator: String::new(),
                created: Instant::now(),
            },
        );
        if outcome.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        outcome?;
        Ok(UploadInfo { id })
    }

    /// Append one chunk at exactly the current received length. Gaps and
    /// overwrites are rejected; resume by reading status and continuing.
    pub fn upload_append(&self, id: &str, offset: u64, bytes: &[u8]) -> Result<u64> {
        if !is_transfer_id(id) {
            return Err(upload_missing());
        }
        if bytes.len() > MAX_CHUNK {
            return Err(Error::invalid());
        }
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        let transfer = match transfers.get_mut(id) {
            Some(t) if t.created.elapsed() < self.transfer_ttl => t,
            _ => {
                let stale = transfers.remove(id);
                drop(transfers);
                if let Some(t) = stale {
                    let _ = std::fs::remove_dir_all(&t.dir);
                }
                return Err(upload_missing());
            }
        };
        let TransferKind::Upload { received, state } = &mut transfer.kind else {
            return Err(upload_missing());
        };
        if *state != UploadState::Open {
            return Err(Error::busy());
        }
        if offset != *received {
            return Err(Error::invalid());
        }
        if *received + bytes.len() as u64 > MAX_TRANSFER_SIZE {
            return Err(Error::new(
                413,
                "result_limit_exceeded",
                "upload exceeds chunked transfer ceiling",
            ));
        }
        use std::io::Write;
        let prior = *received;
        let write = std::fs::OpenOptions::new()
            .append(true)
            .open(&transfer.file)
            .and_then(|mut f| {
                f.write_all(bytes).map_err(|error| {
                    // write_all may persist a prefix before returning an I/O
                    // error. Restore the acknowledged offset so retrying appends
                    // the complete chunk exactly once.
                    let _ = f.set_len(prior);
                    error
                })
            });
        if let Err(error) = write {
            tracing::warn!(error = %error, "upload append failed");
            return Err(Error::internal());
        }
        *received += bytes.len() as u64;
        Ok(*received)
    }

    pub fn upload_status(&self, id: &str) -> Result<u64> {
        match self.lookup_transfer(id)? {
            Some(t) => match t.kind {
                TransferKind::Upload { received, .. } => Ok(received),
                TransferKind::Snapshot { .. } => Err(upload_missing()),
            },
            None => Err(upload_missing()),
        }
    }

    /// Validate and publish an upload under `name`, reusing the atomic
    /// single-shot import path. A failed commit keeps the upload so the
    /// client can fix the target name and retry; abort or TTL releases it.
    pub fn upload_commit(&self, id: &str, name: &str) -> Result<()> {
        let file = {
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            self.purge_expired_locked(&mut transfers);
            let transfer = transfers.get_mut(id).ok_or_else(upload_missing)?;
            let TransferKind::Upload { state, .. } = &mut transfer.kind else {
                return Err(upload_missing());
            };
            if transfer.created.elapsed() >= self.transfer_ttl {
                return Err(upload_missing());
            }
            if *state != UploadState::Open {
                return Err(Error::busy());
            }
            *state = UploadState::Committing;
            transfer.file.clone()
        };

        let result = self.import_from(name, &file);
        if result.is_ok() {
            self.remove_transfer(id);
            return Ok(());
        }

        // A rejected target name or validation failure leaves the complete
        // upload available for a corrected retry, but never concurrent with
        // another append/abort while publication was in progress.
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        if let Some(transfer) = transfers.get_mut(id)
            && let TransferKind::Upload { state, .. } = &mut transfer.kind
        {
            *state = UploadState::Open;
        }
        result
    }

    pub fn upload_abort(&self, id: &str) -> Result<()> {
        let removed = {
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            self.purge_expired_locked(&mut transfers);
            let transfer = transfers.get_mut(id).ok_or_else(upload_missing)?;
            let TransferKind::Upload { state, .. } = &mut transfer.kind else {
                return Err(upload_missing());
            };
            if *state != UploadState::Open {
                return Err(Error::busy());
            }
            transfers.remove(id)
        };
        if let Some(transfer) = removed {
            let _ = std::fs::remove_dir_all(transfer.dir);
        }
        Ok(())
    }

    /// Drop expired transfers and their retained directories. Driven by the
    /// server maintenance loop; begin paths purge inline before counting.
    pub fn sweep_transfers(&self) -> Result<usize> {
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        Ok(self.purge_expired_locked(&mut transfers))
    }

    fn retained_transfer_dir(&self) -> Result<PathBuf> {
        tempfile::Builder::new()
            .prefix(".transfer-")
            .tempdir_in(&self.root)
            .map_err(|_| Error::internal())
            .map(|dir| dir.keep())
    }

    fn insert_transfer(&self, id: String, transfer: Transfer) -> Result<()> {
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        self.purge_expired_locked(&mut transfers);
        if transfers.len() >= MAX_TRANSFERS {
            return Err(Error::busy());
        }
        transfers.insert(id, transfer);
        Ok(())
    }

    fn lookup_transfer(&self, id: &str) -> Result<Option<Transfer>> {
        if !is_transfer_id(id) {
            return Ok(None);
        }
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        match transfers.get(id) {
            Some(t) if t.created.elapsed() < self.transfer_ttl => Ok(Some(t.clone())),
            Some(_) => {
                let stale = transfers.remove(id);
                drop(transfers);
                if let Some(t) = stale {
                    let _ = std::fs::remove_dir_all(&t.dir);
                }
                Ok(None)
            }
            None => Ok(None),
        }
    }

    fn remove_transfer(&self, id: &str) {
        let removed = self
            .transfers
            .lock()
            .map(|mut map| map.remove(id))
            .ok()
            .flatten();
        if let Some(t) = removed {
            let _ = std::fs::remove_dir_all(&t.dir);
        }
    }

    fn purge_expired_locked(&self, transfers: &mut HashMap<String, Transfer>) -> usize {
        let stale: Vec<String> = transfers
            .iter()
            .filter(|(_, t)| t.created.elapsed() >= self.transfer_ttl)
            .map(|(id, _)| id.clone())
            .collect();
        let count = stale.len();
        for id in stale {
            if let Some(t) = transfers.remove(&id) {
                let _ = std::fs::remove_dir_all(&t.dir);
            }
        }
        count
    }
}

fn snapshot_missing() -> Error {
    Error::new(404, "snapshot_not_found", "snapshot unknown or expired")
}

fn upload_missing() -> Error {
    Error::new(404, "upload_not_found", "upload unknown or expired")
}

fn is_transfer_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Best-effort removal of staging directories orphaned by a crash. A new
/// process owns no live transfers, so anything matching the staging prefixes
/// is residue. Failures are logged, never fatal to startup.
fn sweep_orphan_transfer_dirs(root: &Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(error = %error, "transfer orphan sweep skipped");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_staging = name.starts_with(".transfer-") || name.starts_with(".lifecycle-");
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_staging
            && is_dir
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::warn!(path = %entry.path().display(), error = %error, "orphaned staging dir not removed");
        }
    }
}

/// Streaming SHA-256 so hashing a large snapshot never loads it into memory.
fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|_| Error::internal())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(|_| Error::internal())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn migrate_metadata(conn: &mut Connection, root: &Path) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS databases(name TEXT PRIMARY KEY, generation TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS write_markers(name TEXT PRIMARY KEY, written_at INTEGER NOT NULL);",
    )?;
    for (column, declaration) in [
        ("deleted_at", "INTEGER"),
        ("purge_after", "INTEGER"),
        ("created_at", "INTEGER"),
        // Existing registries need a nullable migration step because SQLite
        // cannot add a NOT NULL column to a table that already has rows.
        ("generation", "TEXT"),
    ] {
        let exists: bool = tx
            .prepare("SELECT 1 FROM pragma_table_info('databases') WHERE name=?")?
            .exists([column])?;
        if !exists {
            // Only the explicit absence case is benign. Every ALTER error is
            // returned so startup cannot proceed with a partial registry.
            tx.execute_batch(&format!(
                "ALTER TABLE databases ADD COLUMN {column} {declaration};"
            ))?;
        }
    }
    // Backfill creation timestamps for databases predating the column: file
    // mtime is the best surviving signal, 0 when the file is gone.
    let stale: Vec<String> = tx
        .prepare("SELECT name FROM databases WHERE created_at IS NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for name in stale {
        let stamp = file_mtime_secs(&root.join(format!("{name}.sqlite")));
        tx.execute(
            "UPDATE databases SET created_at=? WHERE name=?",
            rusqlite::params![stamp, name],
        )?;
    }
    // Every registry row must have an opaque identity before credentials are
    // migrated. The identity survives tombstoning and disappears only when
    // the registry row is purged.
    let missing_generations: Vec<String> = tx
        .prepare("SELECT name FROM databases WHERE generation IS NULL OR generation=''")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for name in missing_generations {
        let generation = new_generation(&tx)?;
        tx.execute(
            "UPDATE databases SET generation=? WHERE name=?",
            params![generation, name],
        )?;
    }
    migrate_legacy_scopes(&tx)?;
    tx.commit()?;
    Ok(())
}

fn migrate_legacy_scopes(conn: &Connection) -> Result<()> {
    let credentials: Vec<(String, String)> = conn
        .prepare("SELECT name,databases FROM credentials")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (credential, raw) in credentials {
        let value: Value = serde_json::from_str(&raw).map_err(|_| Error::internal())?;
        let entries = value.as_array().ok_or_else(Error::internal)?;
        // New-format rows are already generation-bound. Parsing validates
        // their shape and rejects malformed metadata rather than broadening
        // it at startup.
        if !entries.iter().all(Value::is_string) {
            parse_scopes(&raw)?;
            continue;
        }
        let mut scopes = Vec::with_capacity(entries.len());
        for entry in entries {
            let name = entry.as_str().ok_or_else(Error::internal)?;
            if validate_name(name).is_err() {
                return Err(Error::internal());
            }
            let generation: Option<String> = conn
                .query_row(
                    "SELECT generation FROM databases WHERE name=?",
                    [name],
                    |r| r.get(0),
                )
                .optional()?;
            // A previously purged scope is retained only as a dropped access
            // grant. Omitting it is safe; inventing a generation is not.
            if let Some(generation) = generation {
                if generation.is_empty() {
                    return Err(Error::internal());
                }
                scopes.push(DatabaseScope {
                    name: name.to_string(),
                    generation,
                });
            }
        }
        let encoded = serde_json::to_string(&scopes).map_err(|_| Error::internal())?;
        conn.execute(
            "UPDATE credentials SET databases=? WHERE name=?",
            params![encoded, credential],
        )?;
    }
    Ok(())
}

fn parse_scopes(raw: &str) -> Result<Vec<DatabaseScope>> {
    let scopes: Vec<DatabaseScope> = serde_json::from_str(raw).map_err(|_| Error::internal())?;
    if scopes
        .iter()
        .any(|scope| scope.generation.is_empty() || validate_name(&scope.name).is_err())
    {
        return Err(Error::internal());
    }
    Ok(scopes)
}

fn scope_names(scopes: &[DatabaseScope]) -> Vec<String> {
    scopes.iter().map(|scope| scope.name.clone()).collect()
}

fn new_generation(conn: &Connection) -> Result<String> {
    for _ in 0..16 {
        let generation = hex::encode(rand::random::<[u8; 16]>());
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE generation=?)",
            [&generation],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(generation);
        }
    }
    Err(Error::internal())
}

fn now_secs() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| Error::internal())
}

fn path_with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn remove_database_files(base: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let p = path_with_suffix(base, suffix);
        let _ = std::fs::remove_file(&p);
    }
}

/// Best-effort file modification time as unix seconds; 0 when unknown.
fn file_mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Live database size: main file plus any WAL sidecars.
fn database_size_bytes(path: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|s| std::fs::metadata(path_with_suffix(path, s)).ok())
        .map(|m| m.len())
        .sum()
}

pub fn unauthorized() -> Error {
    Error::new(
        401,
        "invalid_credential",
        "valid bearer credential required",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_commit_blocks_concurrent_append_and_abort() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let upload = store.upload_begin().unwrap();
        let transfer_dir = store
            .transfers
            .lock()
            .unwrap()
            .get(&upload.id)
            .unwrap()
            .dir
            .clone();
        // A valid standalone SQLite upload lets commit enter the long copy and
        // validation phase while another operation observes its state.
        {
            let db = rusqlite::Connection::open(transfer_dir.join("upload.bin")).unwrap();
            db.execute_batch("CREATE TABLE t(v TEXT); INSERT INTO t VALUES ('ok')")
                .unwrap();
        }
        {
            let mut transfers = store.transfers.lock().unwrap();
            let transfer = transfers.get_mut(&upload.id).unwrap();
            let TransferKind::Upload { state, .. } = &mut transfer.kind else {
                panic!("upload kind")
            };
            *state = UploadState::Committing;
        }
        assert_eq!(
            store.upload_append(&upload.id, 0, b"x").unwrap_err().code,
            "capacity_exceeded"
        );
        assert_eq!(
            store.upload_abort(&upload.id).unwrap_err().code,
            "capacity_exceeded"
        );
        assert!(
            transfer_dir.exists(),
            "commit state must not delete acknowledged bytes"
        );
    }

    #[test]
    fn write_markers_survive_restart_and_older_malformed_set_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("db").unwrap();
        store.note_write("db");
        let before = store.last_write("db").unwrap();
        drop(store);

        let reopened = Store::open(dir.path()).unwrap();
        let after = reopened.last_write("db").unwrap();
        assert_eq!(after, before, "marker persists at whole-second precision");

        let root = dir.path().join("sets");
        std::fs::create_dir_all(root.join("20200101T000000Z")).unwrap();
        std::fs::create_dir_all(root.join("20200101T000001Z")).unwrap();
        std::fs::write(root.join("20200101T000000Z/MANIFEST.json"), "{").unwrap();
        std::fs::write(
            root.join("20200101T000001Z/MANIFEST.json"),
            r#"{"databases":[{"name":"db"}],"registry":{"sha256":"x"}}"#,
        )
        .unwrap();
        let with_root = reopened.with_backup_root(Some(root));
        let sets = with_root.backup_sets().unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].timestamp, "20200101T000001Z");
    }

    #[test]
    fn export_limit_stops_before_full_snapshot_materialization() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("db").unwrap();
        let path = store.path("db").unwrap();
        for sql in [
            "CREATE TABLE t(v TEXT)",
            "INSERT INTO t SELECT hex(randomblob(20000))",
        ] {
            crate::execution::execute(
                &path,
                &[crate::execution::Statement {
                    sql: sql.into(),
                    args: vec![],
                }],
                false,
                false,
                "owner",
                None,
            )
            .unwrap();
        }
        let error = match store.export_snapshot_limited("db", Some(32 * 1024)) {
            Ok(_) => panic!("oversized export unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.code, "result_limit_exceeded");
    }

    #[test]
    fn failed_publication_leaves_no_partial_database_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("src").unwrap();
        let path = store.path("src").unwrap();
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t (v) VALUES ('alpha'),('beta')",
        ] {
            crate::execution::execute(
                &path,
                &[crate::execution::Statement {
                    sql: sql.into(),
                    args: vec![],
                }],
                false,
                false,
                "owner",
                None,
            )
            .unwrap();
        }
        // Copy the snapshot bytes aside and release staging before the
        // failure, so any remaining `.lifecycle-` directory is a real leak.
        let source = dir.path().join("source.sqlite");
        {
            let snapshot = store.export_snapshot("src").unwrap();
            std::fs::copy(snapshot.path(), &source).unwrap();
        }
        // Deterministic publish failure: an orphan file (as left by an
        // interrupted publish) makes the hard_link syscall fail with a real
        // filesystem error after staging validation already passed. No host
        // mounts, permissions, or disk capacity are touched.
        let orphan = store.root.join("blocked.sqlite");
        std::fs::write(&orphan, b"orphan bytes").unwrap();
        let failed = store.import_from("blocked", &source);
        assert_eq!(failed.unwrap_err().code, "database_exists");
        // No partial publication: no registry row, no queryable database,
        // the orphan file is not clobbered, and staging cleaned itself up.
        let meta = Connection::open(store.root.join("metadata.sqlite")).unwrap();
        let rows: i64 = meta
            .query_row(
                "SELECT count(*) FROM databases WHERE name='blocked'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(
            store.path("blocked").unwrap_err().code,
            "database_not_found"
        );
        assert_eq!(std::fs::read(&orphan).unwrap(), b"orphan bytes");
        assert!(
            !std::fs::read_dir(&store.root).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lifecycle-")),
            "failed import must not leak staging directories"
        );
        // Recovery: removing the orphan lets the identical import succeed
        // with exact data.
        std::fs::remove_file(&orphan).unwrap();
        store.import_from("blocked", &source).unwrap();
        let published = store.path("blocked").unwrap();
        let count = crate::execution::execute(
            &published,
            &[crate::execution::Statement {
                sql: "SELECT count(*) FROM t".into(),
                args: vec![],
            }],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(count["rows"][0][0], 2);
    }

    #[test]
    fn failed_create_removes_unregistered_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        let metadata = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        metadata
            .execute_batch(
                "CREATE TRIGGER reject_create BEFORE INSERT ON databases
                 WHEN NEW.name='broken'
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();
        drop(metadata);

        assert!(store.create("broken").is_err());
        assert!(!dir.path().join("broken.sqlite").exists());
        assert_eq!(store.path("broken").unwrap_err().code, "database_not_found");
    }

    #[test]
    fn interrupted_publish_exposes_nothing_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite");
        {
            let store = Store::open(dir.path()).unwrap();
            store.bootstrap().unwrap();
            store.create("src").unwrap();
            let path = store.path("src").unwrap();
            for sql in [
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
                "INSERT INTO t (v) VALUES ('alpha'),('beta')",
            ] {
                crate::execution::execute(
                    &path,
                    &[crate::execution::Statement {
                        sql: sql.into(),
                        args: vec![],
                    }],
                    false,
                    false,
                    "owner",
                    None,
                )
                .unwrap();
            }
            let snapshot = store.export_snapshot("src").unwrap();
            std::fs::copy(snapshot.path(), &source).unwrap();
        }
        // Simulate a crash between file publication (hard_link) and the
        // registry commit: the file exists with no registry row. This is the
        // exact on-disk state of an interrupted import_from.
        let orphan = dir.path().join("interrupted.sqlite");
        std::fs::copy(&source, &orphan).unwrap();
        // Restart: reopen the store against the same directory.
        let store = Store::open(dir.path()).unwrap();
        // No partial exposure: the unregistered file is invisible.
        assert_eq!(
            store.path("interrupted").unwrap_err().code,
            "database_not_found"
        );
        // No silent overwrite or adoption: creation refuses, and the orphan
        // bytes are untouched.
        assert_eq!(
            store.create("interrupted").unwrap_err().code,
            "database_exists"
        );
        assert_eq!(
            std::fs::read(&orphan).unwrap(),
            std::fs::read(&source).unwrap()
        );
        // A later valid operation on another name still succeeds.
        store.import_from("elsewhere", &source).unwrap();
        // Operator recovery: remove the orphan, re-import, exact data.
        std::fs::remove_file(&orphan).unwrap();
        store.import_from("interrupted", &source).unwrap();
        let published = store.path("interrupted").unwrap();
        let count = crate::execution::execute(
            &published,
            &[crate::execution::Statement {
                sql: "SELECT count(*) FROM t".into(),
                args: vec![],
            }],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(count["rows"][0][0], 2);
    }

    #[test]
    fn legacy_scopes_are_migrated_to_current_database_generations() {
        let dir = tempfile::tempdir().unwrap();
        let key = "a".repeat(64);
        let metadata = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        metadata
            .execute_batch(
                "CREATE TABLE credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
                 CREATE TABLE databases(name TEXT PRIMARY KEY);
                 INSERT INTO databases(name) VALUES ('legacy');",
            )
            .unwrap();
        metadata
            .execute(
                "INSERT INTO credentials VALUES ('app',?,'database_read_write','[\"legacy\"]')",
                [hash(&key)],
            )
            .unwrap();
        drop(metadata);

        let store = Store::open(dir.path()).unwrap();
        let principal = store.authenticate(&key).unwrap();
        assert_eq!(principal.databases, vec!["legacy"]);
        assert_eq!(principal.scopes.len(), 1);
        assert!(!principal.scopes[0].generation.is_empty());
        store.authorize(&principal, "legacy").unwrap();
        let metadata = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        let generation: String = metadata
            .query_row(
                "SELECT generation FROM databases WHERE name='legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(generation, principal.scopes[0].generation);
    }

    #[test]
    fn metadata_migration_propagates_backfill_failures() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
             CREATE TABLE databases(name TEXT PRIMARY KEY, deleted_at INTEGER, purge_after INTEGER, created_at INTEGER);
             INSERT INTO databases(name) VALUES ('legacy');
             CREATE TRIGGER reject_backfill BEFORE UPDATE ON databases BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        )
        .unwrap();
        let error = migrate_metadata(&mut conn, dir.path()).unwrap_err();
        assert_eq!(error.code, "invalid_sql");
    }

    #[test]
    fn clone_import_delete_roundtrip_at_store_level() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("src").unwrap();
        let path = store.path("src").unwrap();
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t (v) VALUES ('alpha'),('beta')",
        ] {
            crate::execution::execute(
                &path,
                &[crate::execution::Statement {
                    sql: sql.into(),
                    args: vec![],
                }],
                false,
                false,
                "owner",
                None,
            )
            .unwrap();
        }
        // Clone via store API only.
        let result = store.clone_database("src", "copy1");
        if let Err(ref e) = result {
            panic!("clone failed: {e}");
        }
        let copy = store.path("copy1").unwrap();
        let count = crate::execution::execute(
            &copy,
            &[crate::execution::Statement {
                sql: "SELECT count(*) FROM t".into(),
                args: vec![],
            }],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(count["rows"][0][0], 2);
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;

    #[test]
    fn request_owned_staging_survives_same_name_operations() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("source").unwrap();
        let first = store.export_snapshot("source").unwrap();
        let bytes = std::fs::read(first.path()).unwrap();
        let second = store.export_snapshot("source").unwrap();
        assert_ne!(first.path(), second.path());
        drop(second);
        assert_eq!(std::fs::read(first.path()).unwrap(), bytes);
        let import_a = store.import_staging("target").unwrap();
        std::fs::write(import_a.path(), &bytes).unwrap();
        let import_b = store.import_staging("target").unwrap();
        std::fs::write(import_b.path(), b"invalid").unwrap();
        assert_ne!(import_a.path(), import_b.path());
        assert!(store.import_from("target", import_b.path()).is_err());
        drop(import_b);
        store.import_from("target", import_a.path()).unwrap();
        assert_eq!(std::fs::read(import_a.path()).unwrap(), bytes);
    }

    #[test]
    fn purge_defers_until_inflight_request_closes_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("active").unwrap();
        let request = store.request_lease().unwrap();
        let path = store.path("active").unwrap();
        let mut writer = Connection::open(&path).unwrap();
        let tx = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute_batch("CREATE TABLE pending(v); INSERT INTO pending VALUES (1);")
            .unwrap();
        store.delete("active", 0).unwrap();
        // Actual lease and transaction remain live, not a timing assumption.
        assert!(store.purge_expired().unwrap().is_empty());
        assert!(path.exists());
        assert_eq!(store.path("active").unwrap_err().code, "database_not_found");
        tx.commit().unwrap();
        drop(writer);
        drop(request);
        assert_eq!(store.purge_expired().unwrap(), vec!["active"]);
        store.create("active").unwrap();
        assert!(store.purge_expired().unwrap().is_empty());
        assert!(store.path("active").unwrap().exists());
    }

    #[test]
    fn tombstone_purges_after_retention_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("doomed").unwrap();
        let path = store.path("doomed").unwrap();
        assert!(path.exists());
        // Tombstone with 0-second retention; purge must remove file + row.
        store.delete("doomed", 0).unwrap();
        let purged = store.purge_expired().unwrap();
        assert_eq!(purged, vec!["doomed".to_string()]);
        assert!(
            !path.exists(),
            "file must be physically removed after retention"
        );
        assert!(!path.with_extension("sqlite-wal").exists());
        // Registry row is gone: recreate works.
        store.create("doomed").unwrap();
    }
}
