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
    Read(AdmissionGuard),
    Write(AdmissionGuard),
}

/// Request-pressure counters (observability contract §1): total plus the
/// three shed/degrade signals. Updated at the API layer in `work_in`,
/// which every route funnels through; per-code detail beyond these signals
/// stays in the structured logs, not in memory. Per-database usage lives
/// alongside (same struct, separate map) for the usage views.
pub struct Metrics {
    started: Instant,
    total: AtomicU64,
    capacity_exceeded: AtomicU64,
    deadline_exceeded: AtomicU64,
    internal_error: AtomicU64,
    usage: Mutex<HashMap<String, DbUsage>>,
    queue: Mutex<HashMap<String, DbQueue>>,
}

/// Rows read/written per database since boot. Recorded at the API layer
/// after a successful query/batch (replayed responses count: they are
/// served requests). Plain totals under one short mutex, mirroring
/// `writes`. Keyed by database name, so the map is bounded by the number of
/// DISTINCT names seen since boot, not by the live registry cap: a
/// create/purge cycle on a fresh name leaves an entry behind unless it is
/// pruned. `Metrics::forget` drops a purged name's entry, called only under
/// the exclusive lifecycle lease.
#[derive(Debug, Default, Clone, Copy)]
pub struct DbUsage {
    pub rows_read: u64,
    pub rows_written: u64,
}

/// Per-database admission queue state.
///
/// A ticket lock, not a phase flag. `serving` names the only ticket allowed
/// to look at the queue next, so admission is decided by ARRIVAL ORDER and
/// not by which thread happens to win the state mutex. The two properties
/// that make it work:
///
/// * `serving` only ever advances, and a ticket whose owner gave up is
///   recorded in `abandoned` so the tickets behind it can step over it.
/// * A reader may enter only while no writer holds a SMALLER ticket. Since
///   tickets are handed out on arrival, the readers already inside plus the
///   readers arriving before the writer form one finite batch, and no reader
///   arriving after the writer can get in ahead of it. That is the
///   writer-preferring bound, and it holds no matter how hard the read flood
///   pushes or how fast the client re-issues.
#[derive(Debug, Default)]
struct QueueState {
    /// Readers currently holding the shared guard. Any number may hold it at
    /// once; the queue bounds the WRITER, not reader concurrency.
    readers_active: usize,
    writer_active: bool,
    /// Next ticket to hand out, and the ticket allowed to proceed next.
    next_ticket: u64,
    serving: u64,
    /// Tickets held by writers not yet admitted. A reader is held back by
    /// the earliest of these.
    waiting_writers: BTreeSet<u64>,
    /// Tickets whose owner hit ACQUIRE_TIMEOUT and left. Stepped over so one
    /// give-up cannot wedge every ticket behind it.
    abandoned: BTreeSet<u64>,
}

impl QueueState {
    /// Step `serving` over tickets that were given up. Without this a single
    /// abandoned ticket would stall the queue permanently, which is the
    /// wedge this design exists to avoid.
    fn skip_abandoned(&mut self) {
        while self.abandoned.remove(&self.serving) {
            self.serving += 1;
        }
    }

    /// Whether the reader holding `ticket` may take the shared guard.
    ///
    /// The bound is carried by `serving == ticket` on its own: a writer that
    /// arrived earlier still sitting in `waiting_writers` is holding the
    /// gate at its own smaller ticket, so `serving` cannot have reached this
    /// reader. The third clause is therefore a REDUNDANT restatement of that,
    /// kept because it names the property where a reader of this code looks
    /// for it and because it would catch a future change that dropped the
    /// ticket check. It must not be read as the mechanism: mutating it alone
    /// changes no behaviour, which is exactly what makes it redundant.
    fn reader_may_enter(&self, ticket: u64) -> bool {
        !self.writer_active
            && self.serving == ticket
            && self
                .waiting_writers
                .first()
                .is_none_or(|ahead| *ahead > ticket)
    }

    /// Whether the writer holding `ticket` may take the exclusive guard.
    fn writer_may_enter(&self, ticket: u64) -> bool {
        !self.writer_active && self.readers_active == 0 && self.serving == ticket
    }
}

/// A per-database admission queue.
///
/// Three designs were implemented and measured here, and the two cheap ones
/// were both wrong, which is why this is a ticket lock:
///
/// * The original polled `tokio::sync::RwLock` every 2ms. A writer that
///   loses `try_write_owned` is NOT enqueued in the lock's own fair queue,
///   so it is invisible to the lock and readers released by the global read
///   lane keep taking the shared guard before its next poll. Measured at a
///   7.8x writer rejection rate under a read flood (0.09% -> 0.70%).
/// * A writer-preferring flag over that same RwLock fixed the writers
///   (0.70% -> 0.075%) and starved the readers: 4,260 reader rejections
///   against 5 writer rejections in one run. A bound that moves the queue
///   sideways is not a bound.
/// * A phase-fair lock that closed the read phase for a queued writer was
///   right about who wins a phase and wrong about what decides it. Its writer
///   test ignored `writer_phase` entirely, so a writer was admitted at
///   whatever instant one of its threads happened to observe
///   `readers_active == 0`, while `refresh_phase` handed the read phase to
///   any queued reader at each batch boundary. That is a race, not a bound:
///   writer admission is decided by thread scheduling rather than by queue
///   order. Measured outcomes on the same box disagreed by construction --
///   0-1 of 48 writer attempts admitted over three runs on one harness,
///   essentially all attempts admitted on another, 2 of 358 in an earlier
///   audit -- so the rate is a property of the harness, not of the design.
///   The defect is structural and does not depend on which number you get:
///   nothing in that design guaranteed a queued writer's turn.
///
/// This one has no phases to race. Admission is decided by ticket order, a
/// queued writer is admitted at the next reader batch boundary, and neither
/// side can starve the other. Full runs in
/// docs/soak-starvation-evidence.log.
#[derive(Debug, Default)]
pub struct AdmissionQueue {
    state: Mutex<QueueState>,
    available: Condvar,
}

/// Held for the duration of one admitted request. Releases the read or write
/// slot on drop and wakes whoever the ticket order now favours.
pub struct AdmissionGuard {
    queue: Arc<AdmissionQueue>,
    write: bool,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let Ok(mut state) = self.queue.state.lock() else {
            // A poisoned state lock is already a request-failing condition
            // everywhere else; dropping the notification cannot make it
            // worse, and panicking in a destructor would abort the process.
            return;
        };
        if self.write {
            state.writer_active = false;
        } else {
            state.readers_active = state.readers_active.saturating_sub(1);
        }
        self.queue.available.notify_all();
    }
}

impl AdmissionQueue {
    /// Admit one reader or writer, or give up at ACQUIRE_TIMEOUT.
    fn acquire(queue: &Arc<Self>, write: bool) -> Result<AdmissionGuard> {
        // Ticket and deadline are taken under ONE lock acquisition. Taking
        // the deadline first and the ticket second would let the two orders
        // invert, and a writer that abandoned a ticket above `serving` would
        // stall every ticket behind it.
        let mut state = queue.state.lock().map_err(|_| Error::internal())?;
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        if write {
            state.waiting_writers.insert(ticket);
        }
        loop {
            state.skip_abandoned();
            let admitted = if write {
                if state.writer_may_enter(ticket) {
                    state.writer_active = true;
                    state.waiting_writers.remove(&ticket);
                    state.serving = ticket + 1;
                    true
                } else {
                    false
                }
            } else if state.reader_may_enter(ticket) {
                state.readers_active += 1;
                // Hand the gate on so the next reader in the batch joins
                // immediately instead of one mutex round trip per reader.
                state.serving = ticket + 1;
                true
            } else {
                false
            };
            if admitted {
                queue.available.notify_all();
                return Ok(AdmissionGuard {
                    queue: queue.clone(),
                    write,
                });
            }
            let (guard, keep_going) = AdmissionQueue::wait(queue, state, deadline)?;
            state = guard;
            if !keep_going {
                state.abandoned.insert(ticket);
                state.waiting_writers.remove(&ticket);
                state.skip_abandoned();
                queue.available.notify_all();
                return Err(Error::busy());
            }
        }
    }

    /// Wait for a state change. Returns the guard and whether the caller
    /// should keep trying; false means ACQUIRE_TIMEOUT has passed. A
    /// poisoned lock is reported rather than recovered, matching how the
    /// rest of the store treats a panic that escaped a critical section.
    fn wait<'a>(
        queue: &'a AdmissionQueue,
        state: std::sync::MutexGuard<'a, QueueState>,
        deadline: Instant,
    ) -> Result<(std::sync::MutexGuard<'a, QueueState>, bool)> {
        let now = Instant::now();
        if now >= deadline {
            return Ok((state, false));
        }
        let (guard, _) = queue
            .available
            .wait_timeout(state, deadline - now)
            .map_err(|_| Error::internal())?;
        Ok((guard, Instant::now() < deadline))
    }
}

/// Per-database admission-queue waits since boot. These counters are the
/// starvation measurement that settled scale Phase 2, and they are also how
/// the bound is kept honest: under a read flood on one database `timeouts`
/// should stay flat, and sustained growth means the writer-preferring bound
/// in `AdmissionQueue` is not holding.
#[derive(Debug, Default, Clone, Copy)]
pub struct DbQueue {
    pub waits: u64,
    pub wait_ms: u64,
    pub timeouts: u64,
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
            usage: Mutex::new(HashMap::new()),
            queue: Mutex::new(HashMap::new()),
        }
    }
    pub fn record_queue_wait(&self, name: &str, waited: Duration, timed_out: bool) {
        if let Ok(mut queue) = self.queue.lock() {
            let entry = queue.entry(name.to_string()).or_default();
            entry.waits += 1;
            entry.wait_ms += waited.as_millis() as u64;
            if timed_out {
                entry.timeouts += 1;
            }
        }
    }
    pub fn queue_snapshot(&self) -> HashMap<String, DbQueue> {
        self.queue
            .lock()
            .map(|queue| queue.clone())
            .unwrap_or_default()
    }
    pub fn record_usage(&self, name: &str, rows_read: u64, rows_written: u64) {
        if let Ok(mut usage) = self.usage.lock() {
            let entry = usage.entry(name.to_string()).or_default();
            entry.rows_read += rows_read;
            entry.rows_written += rows_written;
        }
    }
    /// Drop the per-name counters for a purged database. Only safe under the
    /// exclusive lifecycle lease, which every admitted request holds in
    /// shared form, so no in-flight request can still be recording or
    /// reading this name's entry.
    pub fn forget(&self, name: &str) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.remove(name);
        }
        if let Ok(mut usage) = self.usage.lock() {
            usage.remove(name);
        }
    }
    pub fn usage_snapshot(&self) -> HashMap<String, DbUsage> {
        self.usage
            .lock()
            .map(|usage| usage.clone())
            .unwrap_or_default()
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
pub(crate) fn is_set_dir(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 16
        && b[..8].iter().all(|c| c.is_ascii_digit())
        && b[8] == b'T'
        && b[9..15].iter().all(|c| c.is_ascii_digit())
        && b[15] == b'Z'
}

/// One published versioned set (scripts/micro2-backup.sh layout): directory
/// name, filesystem mtime, member database names, registry presence, and the
/// conservative start boundary captured before any database export. A set may
/// be published after a concurrent write, so publication mtime is not proof
/// that the write was included; only a write strictly before this boundary is
/// eligible to be reported fresh.
pub struct BackupSet {
    pub timestamp: String,
    pub mtime: SystemTime,
    pub coverage_start: Option<SystemTime>,
    pub coverage_end: Option<SystemTime>,
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

    /// Start a durable freshness interval before a mutation begins. The
    /// negative marker is intentionally written first: if the process dies
    /// after the data commit but before the positive marker, restart reports
    /// `unknown` rather than falsely fresh. If this fence cannot be persisted,
    /// the mutation is rejected before it can commit.
    pub fn begin_write(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _marker_guard = self.marker_lock.lock().map_err(|_| Error::internal())?;
        self.persist_marker_value(name, -1)?;
        if let Ok(mut writes) = self.writes.lock() {
            writes.remove(name);
        }
        Ok(())
    }

    /// Close a successful mutation's freshness interval. If the positive
    /// marker cannot be persisted, the pre-write negative marker remains, so
    /// backup status stays conservative after the mutation and after restart.
    pub fn finish_write(&self, name: &str) {
        let stamp = SystemTime::now();
        let seconds = stamp
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(i64::MAX);
        let Ok(_marker_guard) = self.marker_lock.lock() else {
            tracing::warn!(database = name, "backup write marker lock poisoned");
            if let Ok(mut writes) = self.writes.lock() {
                writes.remove(name);
            }
            return;
        };
        if self.persist_marker_value(name, seconds).is_ok() {
            if let Ok(mut writes) = self.writes.lock() {
                writes.insert(name.to_string(), stamp);
            }
        } else {
            tracing::warn!(
                database = name,
                "backup write marker persistence failed; freshness remains unknown"
            );
            if let Ok(mut writes) = self.writes.lock() {
                writes.remove(name);
            }
        }
    }

    /// Undo a `begin_write` fence for a mutation that rolled back without
    /// committing anything. The fence exists for crash-safety (die after the
    /// data commit but before the positive marker means `unknown`, never
    /// falsely fresh), but a rejected write changed nothing, so leaving the
    /// `-1` marker would flip a fresh database to `unknown` and fire a false
    /// Alert digest on the next poll. Callers capture `last_write` before
    /// `begin_write` and restore it here on the failure path.
    pub fn cancel_write(&self, name: &str, previous: Option<SystemTime>) {
        let Ok(_marker_guard) = self.marker_lock.lock() else {
            return;
        };
        if let Ok(conn) = self.marker_db.lock() {
            match previous {
                Some(stamp) => {
                    let seconds = stamp
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.as_secs() as i64)
                        .unwrap_or(i64::MAX);
                    let _ = conn.execute(
                        "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                         ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                        params![name, seconds],
                    );
                }
                None => {
                    let _ = conn.execute("DELETE FROM write_markers WHERE name=?", params![name]);
                }
            }
        }
        if let Ok(mut writes) = self.writes.lock() {
            match previous {
                Some(stamp) => {
                    writes.insert(name.to_string(), stamp);
                }
                None => {
                    writes.remove(name);
                }
            }
        }
    }

    /// Record a successful mutation for legacy store-level callers. New API
    /// mutations use `begin_write` before the write and `finish_write` after.
    #[cfg(test)]
    pub fn note_write_legacy(&self, name: &str) -> Result<()> {
        self.begin_write(name)?;
        self.finish_write(name);
        Ok(())
    }

    pub fn create_and_note(&self, name: &str) -> Result<()> {
        let previous = self.last_write(name);
        self.begin_write(name)?;
        let result = self.create(name);
        if result.is_ok() {
            self.finish_write(name);
        } else {
            self.cancel_write(name, previous);
        }
        result
    }

    pub fn last_write(&self, name: &str) -> Option<SystemTime> {
        let _marker_guard = self.marker_lock.lock().ok()?;
        self.writes
            .lock()
            .ok()
            .and_then(|writes| writes.get(name).copied())
    }

    fn persist_marker_value(&self, name: &str, seconds: i64) -> Result<()> {
        // Marker writes are tiny, but the same SQLite metadata file is also
        // used by authentication and lifecycle operations. Serialize our own
        // updates and retry transient busy/locked failures instead of merely
        // logging a warning after the data write has already committed.
        let conn = self.marker_db.lock().map_err(|_| Error::internal())?;
        let mut delay = Duration::from_millis(2);
        let mut last_error = Error::internal();
        for attempt in 0..5 {
            match conn.execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, seconds],
            ) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error = error.into();
                    if attempt < 4 {
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_millis(80));
                    }
                }
            }
        }
        Err(last_error)
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
            // The canonical producer writes lowercase `manifest.json`; the
            // uppercase name is accepted only as a compatibility reader for
            // older local sets. A missing or malformed member skips just this
            // set, never the whole scan.
            let manifest_path = ["manifest.json", "MANIFEST.json"]
                .into_iter()
                .map(|name| entry.path().join(name))
                .find(|path| path.is_file());
            let Some(manifest_path) = manifest_path else {
                continue;
            };
            let Some(bytes) = read_manifest_capped(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let coverage_start = manifest["started_at"]
                .as_u64()
                .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
            let coverage_end = manifest["completed_at"]
                .as_u64()
                .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
            let databases = manifest["databases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let registry_file = manifest["registry"]["file"].as_str();
            let has_registry = manifest["registry"]["sha256"].as_str().is_some()
                && registry_file.is_some_and(|file| {
                    !file.contains('/') && !file.contains('\\') && entry.path().join(file).is_file()
                });
            sets.push(BackupSet {
                timestamp: name,
                mtime,
                coverage_start,
                coverage_end,
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
        let mut metadata = Connection::open(root.join("metadata.sqlite"))?;
        metadata.busy_timeout(Duration::from_millis(250))?;
        metadata.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA cache_size=-1024;",
        )?;
        migrate_metadata(&mut metadata, root)?;
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
        };
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
        .map_err(map_issue_error)?;
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
        let changed = self.meta()?.execute(
            "DELETE FROM credentials WHERE name=? AND role != 'owner'",
            [name],
        )?;
        if changed == 0 {
            return Err(Error::new(
                404,
                "key_not_found",
                "no such credential; nothing revoked",
            ));
        }
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

    /// Every live database, unscoped: the owner view, and the view backup
    /// freshness needs. Deliberately a separate statement from `list` rather
    /// than a shared one with the filter applied later -- `list` has to carry
    /// the generation column to keep a key bound to one generation of a
    /// name from seeing its successor, and folding that away to share code
    /// would quietly widen what an app key can read.
    pub fn list_all(&self) -> Result<Vec<String>> {
        let conn = self.meta()?;
        let mut stmt =
            conn.prepare("SELECT name FROM databases WHERE deleted_at IS NULL ORDER BY name")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }
    /// Owner-visible database facts: size, user tables with row counts,
    /// creation time, and tombstone state. Tombstoned names report their
    /// retention deadline instead of table detail; unknown names are 404.
    /// COUNT(*) is a per-page C walk (OP_Count), not a per-row scan, so it
    /// stays fast even on large tables; a VDBE progress deadline cannot
    /// bound it (no loop-edge opcodes) and is deliberately absent.
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
            // Same publication ordering as import_from: the directory entry
            // that create_new just made must be durable before the registry
            // row naming it is committed, or a crash can leave the registry
            // pointing at a file that does not exist.
            std::fs::File::open(&path)
                .and_then(|handle| handle.sync_all())
                .map_err(|_| Error::internal())?;
            sync_parent_dir(&path)?;
            let created_at = now_secs()?;
            tx.execute(
                "INSERT INTO databases (name, created_at, generation) VALUES (?, ?, ?)",
                rusqlite::params![name, created_at, generation],
            )?;
            tx.execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, created_at],
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
    pub fn queue(&self, name: &str) -> Result<Arc<AdmissionQueue>> {
        // Call only after path() verifies a bounded registry entry.
        let mut queues = self.queues.lock().map_err(|_| Error::internal())?;
        Ok(queues
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(AdmissionQueue::default()))
            .clone())
    }

    /// Per-database admission with bounded waiting. Readers take the shared
    /// guard (WAL allows concurrent readers against one writer); writers
    /// take the exclusive guard. Blocks on the queue's condvar, so it must
    /// run on a blocking thread and not on an async worker; call only after
    /// path() verifies a bounded registry entry, same as queue(). Waits stay
    /// capped at ACQUIRE_TIMEOUT and every wait is recorded for the
    /// starvation measurement (`DbQueue`, served by `/v1/status`).
    ///
    /// The queue is a fair, writer-preferring ticket lock; see
    /// `AdmissionQueue` for the three designs measured here and why two were
    /// rejected. In short: admission follows arrival order, so a read flood
    /// cannot delay a writer (no reader arriving after a queued writer can
    /// take the shared guard ahead of it) and a writer stream cannot starve
    /// readers (a reader waits only on finitely many earlier tickets).
    pub fn acquire_queue(&self, name: &str, write: bool) -> Result<QueueGuard> {
        let queue = self.queue(name)?;
        let start = Instant::now();
        let result = AdmissionQueue::acquire(&queue, write).map(|guard| {
            if write {
                QueueGuard::Write(guard)
            } else {
                QueueGuard::Read(guard)
            }
        });
        match &result {
            Ok(_) => self.metrics.record_queue_wait(name, start.elapsed(), false),
            Err(_) => self.metrics.record_queue_wait(name, start.elapsed(), true),
        }
        result
    }

    /// Whether a live registry row exists for `name`. Fails closed to
    /// `true`: callers use this to decide whether per-name state may be
    /// released, and leaving the state alone is the safe answer when the
    /// registry cannot be read.
    pub fn registered(&self, name: &str) -> bool {
        let Ok(meta) = self.meta() else {
            return true;
        };
        meta.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE name=? AND deleted_at IS NULL)",
            [name],
            |r| r.get(0),
        )
        .unwrap_or(true)
    }

    /// Release the per-name admission and metric state for a name with no
    /// live registry row. `queue()` allocates on first sight and only a
    /// physical purge reclaims, so a name admitted by a request that then
    /// failed without creating it (rejected delete, bad import payload,
    /// missing clone source) would otherwise pin an `AdmissionQueue` and a
    /// phantom `/v1/status` row forever; no later request revisits it.
    /// Names that do exist are left alone.
    pub fn release_unused(&self, name: &str) {
        if self.registered(name) {
            return;
        }
        if let Ok(mut queues) = self.queues.lock() {
            queues.remove(name);
        }
        self.metrics.forget(name);
    }

    /// Run a write under the freshness fence, releasing the per-name state
    /// again when the operation failed without creating the database. Same
    /// begin/finish/cancel sequence the route handlers ran inline, with the
    /// release on every failure path (including a failed `begin_write`) so
    /// the invariant holds in one place instead of at each handler.
    pub fn fenced_write<F>(&self, name: &str, operation: F) -> Result<()>
    where
        F: FnOnce(&Self) -> Result<()>,
    {
        let previous_write = self.last_write(name);
        if let Err(error) = self.begin_write(name) {
            self.release_unused(name);
            return Err(error);
        }
        let result = operation(self);
        if result.is_ok() {
            self.finish_write(name);
        } else {
            self.cancel_write(name, previous_write);
            self.release_unused(name);
        }
        result
    }

    /// SQLite online backup of `name` to `dest`, consistent during writes.
    /// Pages are copied in bounded steps so concurrent commits interleave;
    /// the snapshot reflects one committed point in time.
    pub fn export_to(&self, name: &str, dest: &Path) -> Result<()> {
        // All lifecycle snapshots share the documented transfer ceiling. The
        // old unbounded call still gets the same protection as clone/export.
        self.export_to_limited(name, dest, Some(MAX_TRANSFER_SIZE))
    }

    /// SQLite online backup with an optional hard output ceiling. The check
    /// runs during backup steps so an over-limit snapshot is not fully
    /// materialized before the caller receives its 413. A `None` ceiling is
    /// treated as the lifecycle ceiling rather than as an unbounded escape.
    pub fn export_to_limited(&self, name: &str, dest: &Path, max_bytes: Option<u64>) -> Result<()> {
        const PAGES_PER_STEP: i32 = 64;
        const RETRY_STEPS: u64 = 64;
        const MAX_BUSY_RETRIES: u32 = 16;
        const RETRY_DELAY: Duration = Duration::from_millis(10);

        let src_path = self.path(name)?;
        // Atomically claim the destination: a check-then-open lets two
        // concurrent exports interleave on one path (or a swap land
        // between the check and SQLite's open). create_new makes the
        // loser a 409 deterministically.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
        {
            Ok(claimed) => drop(claimed),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::new(
                    409,
                    "file_exists",
                    "destination file already exists",
                ));
            }
            Err(_) => return Err(Error::internal()),
        }
        let limit = max_bytes.unwrap_or(MAX_TRANSFER_SIZE);
        // Deliberately no per-database write-slot acquisition: the SQLite
        // online backup API on a WAL source produces a consistent committed
        // snapshot while writes continue (the copy restarts if the source
        // changes between steps). Bound retries and derive the step budget from
        // the byte ceiling and the source's actual page size instead of
        // assuming the default 4096-byte SQLite page.
        let src =
            Connection::open_with_flags(&src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        src.busy_timeout(Duration::from_millis(250))?;
        // A read transaction gives the online-backup source a stable page
        // count for this invocation. Individual steps still use SQLite's
        // online-backup protocol, so committed writers may proceed between
        // steps; the limit is enforced against the observed output as well as
        // the source page count.
        let page_size = src.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?;
        let page_size = u64::try_from(page_size).map_err(|_| Error::internal())?;
        if !(512..=65536).contains(&page_size) {
            return Err(Error::internal());
        }
        let bytes_per_step = page_size.saturating_mul(PAGES_PER_STEP as u64).max(1);
        let max_steps = limit
            .saturating_add(bytes_per_step - 1)
            .checked_div(bytes_per_step)
            .unwrap_or(0)
            .saturating_add(RETRY_STEPS);
        let mut dst = Connection::open(dest)?;
        dst.busy_timeout(Duration::from_millis(250))?;

        let outcome = (|| -> Result<()> {
            let backup = Backup::new(&src, &mut dst)?;
            let mut result = StepResult::More;
            let mut steps = 0u64;
            let mut busy_retries = 0u32;
            let mut failure = None;
            while matches!(
                result,
                StepResult::More | StepResult::Busy | StepResult::Locked
            ) {
                if steps >= max_steps {
                    failure = Some(Error::new(
                        429,
                        "capacity_exceeded",
                        "backup could not complete within bounded transfer work",
                    ));
                    break;
                }
                steps += 1;
                result = match backup.step(PAGES_PER_STEP) {
                    Ok(result) => result,
                    Err(error) => {
                        failure = Some(error.into());
                        break;
                    }
                };
                if matches!(result, StepResult::Busy | StepResult::Locked) {
                    if busy_retries >= MAX_BUSY_RETRIES {
                        failure = Some(Error::new(
                            429,
                            "capacity_exceeded",
                            "backup could not acquire a consistent source snapshot",
                        ));
                        break;
                    }
                    busy_retries += 1;
                    thread::sleep(RETRY_DELAY);
                    continue;
                }
                busy_retries = 0;
                let progress = backup.progress();
                let expected = (progress.pagecount.max(0) as u64).saturating_mul(page_size);
                let observed = std::fs::metadata(dest)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                if expected > limit || observed > limit {
                    failure = Some(Error::new(
                        413,
                        "result_limit_exceeded",
                        "export exceeds bounded response size",
                    ));
                    break;
                }
            }
            drop(backup);
            if let Some(error) = failure {
                return Err(error);
            }
            match result {
                StepResult::Done => {
                    // The final step can be the first one to cross the limit.
                    if std::fs::metadata(dest)
                        .map(|metadata| metadata.len() > limit)
                        .unwrap_or(false)
                    {
                        return Err(Error::new(
                            413,
                            "result_limit_exceeded",
                            "export exceeds bounded response size",
                        ));
                    }
                    dst.pragma_update(None, "journal_mode", "DELETE")?;
                    Ok(())
                }
                _ => Err(Error::new(
                    429,
                    "capacity_exceeded",
                    "backup could not complete; source busy",
                )),
            }
        })();
        if outcome.is_err() {
            drop(dst);
            remove_database_files(dest);
        }
        outcome
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
                Error::new(409, "already_deleted", "database already deleted")
            } else {
                Error::new(404, "database_not_found", "database not found")
            });
        }
        tx.execute(
            "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
             ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
            params![name, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Physically remove files for tombstones whose retention elapsed.
    /// Idempotent, driven by the server maintenance loop even when idle.
    /// Skip a busy tick; never unlink while any request owns a connection.
    pub fn purge_expired(&self) -> Result<Vec<String>> {
        let _exclusive = match self.lifecycle.try_write() {
            Ok(lease) => lease,
            // A contended tick is normal, not an error: the 1s cadence means
            // the next tick almost always succeeds. But a tombstone can then
            // sit past its retention window with no trace, so record the
            // deferral. Debug level keeps it free when nothing is wrong.
            Err(std::sync::TryLockError::WouldBlock) => {
                tracing::debug!("purge tick deferred; lifecycle lease busy");
                return Ok(Vec::new());
            }
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
            // The freshness marker is keyed by the same name, so a purge that
            // leaves the row behind grows metadata.sqlite without bound and
            // reloads every marker into memory on the next boot.
            tx.execute("DELETE FROM write_markers WHERE name=?", [&name])?;
            purged.push(name);
        }
        tx.commit()?;
        // Prune the per-name in-memory state for the names just purged. Safe
        // here and only here: the exclusive lifecycle lease held above is
        // shared by every admitted request, so none can be mid-admission with
        // this name's queue guard in hand. Removing the entry cannot split a
        // guard, because no other holder can exist at this point.
        for name in &purged {
            if let Ok(mut queues) = self.queues.lock() {
                queues.remove(name);
            }
            self.metrics.forget(name);
        }
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
        let path = self.root.join(format!("{name}.sqlite"));
        // Reject an already-full registry before copying an untrusted source.
        // The authoritative check is repeated in the publication transaction.
        {
            let conn = self.meta()?;
            let count: i64 = conn.query_row("SELECT count(*) FROM databases", [], |r| r.get(0))?;
            if count >= MAX_DATABASES {
                return Err(Error::busy());
            }
        }
        let staged = self.staging()?;
        // The import source may be a live SQLite database with a populated WAL
        // sidecar. Copying only the main file would validate an older committed
        // image and silently omit newer rows. The read-only backup API folds
        // the main database and its live sidecars into one consistent image;
        // it also makes the staged copy standalone before publication.
        let source_conn =
            Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|_| Error::invalid())?;
        let mut staged_conn = Connection::open(staged.path()).map_err(|_| Error::internal())?;
        let backup = Backup::new(&source_conn, &mut staged_conn).map_err(|_| Error::invalid())?;
        match backup.step(-1).map_err(|_| Error::invalid())? {
            StepResult::Done => {}
            _ => return Err(Error::invalid()),
        }
        drop(backup);
        drop(staged_conn);
        crate::execution::validate_import(staged.path())?;
        crate::execution::initialize(staged.path())?;

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
        // Publication durability ordering, mirroring publish_export_file in
        // src/main.rs: make the staged bytes durable, link the name, fsync
        // the containing directory so the new entry is stable, and only then
        // commit the registry row. Without the directory fsync a crash can
        // leave the registry naming a database whose file never reached
        // stable storage, and `path()` then reports a registered database as
        // missing with no way to recover it short of a manual registry edit.
        std::fs::File::open(staged.path())
            .and_then(|file| file.sync_all())
            .map_err(|_| Error::internal())?;
        if std::fs::hard_link(staged.path(), &path).is_err() {
            // The link never appeared, so this process published nothing and
            // owns no file at `path`. Return the conflict and touch nothing:
            // a pre-existing file belongs to whoever put it there, and
            // create() applies the same rule ("an unregistered file needs
            // operator recovery, not silent adoption").
            return Err(Error::new(
                409,
                "database_exists",
                "database file exists or cannot be published",
            ));
        }
        // The link exists; now make its directory entry durable. This must
        // succeed BEFORE the registry row that names it is committed, or a
        // crash in between leaves the registry pointing at a file that never
        // reached stable storage -- `path()` only checks is_file(), so there
        // is no self-healing and no signal beyond a 500. Because the link is
        // ours, a failure here means we unlink it rather than stranding an
        // unregistered file that would block a later import of this name.
        if let Err(error) = sync_parent_dir(&path) {
            remove_database_files(&path);
            return Err(error);
        }
        let result = (|| {
            tx.execute(
                "INSERT INTO databases (name, created_at, generation) VALUES (?, ?, ?)",
                rusqlite::params![name, now_secs()?, generation],
            )?;
            tx.execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, now_secs()?],
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
            remove_staging_dir(&dir);
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
        // Lookup under the lock, clone what the read needs, then release
        // before touching the filesystem: holding the registry mutex across
        // file open/seek/read serialized every transfer operation (appends,
        // status, aborts, other chunks) behind one slow 1 MiB read. The
        // post-read revalidation below closes the lookup-vs-read race the
        // coarse lock used to cover: a concurrent release (or TTL purge)
        // between lookup and open reports an honest 404 instead of a 500.
        let (file_path, take, size, sha256) = {
            let transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            let transfer = match transfers.get(id) {
                Some(entry) if entry.created.elapsed() < self.transfer_ttl => entry.clone(),
                Some(_) => return Err(snapshot_missing()),
                None => return Err(snapshot_missing()),
            };
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
            if length == 0 {
                return Err(Error::invalid());
            }
            let take = (length as u64).min(MAX_CHUNK as u64).min(size - offset) as usize;
            (transfer.file.clone(), take, size, sha256)
        };
        // Stale-entry directory removal happens under the lock in the
        // callers that own expiry; here an expired-or-released transfer
        // simply reports 404 without touching the filesystem.
        let mut file = match std::fs::File::open(&file_path) {
            Ok(file) => file,
            Err(_) => {
                // A concurrent release removes the retained directory
                // between lookup and open: report 404 when the transfer is
                // gone, 500 only when it is still registered.
                let gone = self
                    .transfers
                    .lock()
                    .map(|transfers| transfers.get(id).is_none())
                    .unwrap_or(false);
                return Err(if gone {
                    snapshot_missing()
                } else {
                    Error::internal()
                });
            }
        };
        use std::io::{Read, Seek};
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(|_| Error::internal())?;
        let mut bytes = vec![0u8; take];
        file.read_exact(&mut bytes).map_err(|_| Error::internal())?;
        // Revalidate: the transfer must still be registered, unexpired,
        // and unchanged after the read, or the bytes may come from a
        // released directory and must not be served.
        {
            let transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            match transfers.get(id) {
                Some(entry)
                    if entry.created.elapsed() < self.transfer_ttl
                        && entry.file == file_path
                        && (entry.creator == principal.hash || principal.role == "owner") =>
                {
                    match &entry.kind {
                        TransferKind::Snapshot {
                            size: live_size,
                            sha256: live_sha,
                        } if *live_size == size && *live_sha == sha256 => {}
                        _ => return Err(snapshot_missing()),
                    }
                }
                _ => return Err(snapshot_missing()),
            }
        }
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
            remove_staging_dir(&dir);
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
                    remove_staging_dir(&t.dir);
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
                f.write_all(bytes).inspect_err(|_error| {
                    // write_all may persist a prefix before returning an I/O
                    // error. Restore the acknowledged offset so retrying appends
                    // the complete chunk exactly once.
                    let _ = f.set_len(prior);
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
        self.restore_open_after_failed_commit(id, &mut transfers);
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
            remove_staging_dir(&transfer.dir);
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
                    remove_staging_dir(&t.dir);
                }
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Restore a `Committing` upload to `Open` after a failed publication,
    /// so the client can retry with a corrected target name. An upload whose
    /// TTL elapsed while publication ran is NOT revived: the entry is
    /// already gone (purged below) and reviving it would make a dead
    /// upload look resumable until the next append/abort re-checks the TTL
    /// and 404s. A rejected name is recoverable; an expired upload is not,
    /// and the two must not look alike.
    fn restore_open_after_failed_commit(
        &self,
        id: &str,
        transfers: &mut HashMap<String, Transfer>,
    ) {
        self.purge_expired_locked(transfers);
        if let Some(transfer) = transfers.get_mut(id)
            && let TransferKind::Upload { state, .. } = &mut transfer.kind
            && transfer.created.elapsed() < self.transfer_ttl
        {
            *state = UploadState::Open;
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
            remove_staging_dir(&t.dir);
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
                remove_staging_dir(&t.dir);
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

/// Remove a transfer or lifecycle staging directory, recording a failure
/// instead of discarding it.
///
/// A silently swallowed `remove_dir_all` error is the defect this closes:
/// the caller has already removed the in-memory entry, so the directory is
/// unreachable by any later `remove` of the same id, and the orphan sweep
/// only runs at process start — so a single transient EACCES/EBUSY turned
/// into a permanent leak of up to 128 MiB per transfer, with `MAX_TRANSFERS`
/// counting only *live* entries and never noticing. Retrying is deliberately
/// not attempted here (the caller may hold a lock and the caller's own error
/// path must not be blocked by a filesystem retry), and the error is NOT
/// propagated: a failed cleanup of a directory whose data is already
/// unreachable must not fail the request that asked for the removal. What it
/// must not do is be invisible, so it is logged at warn with both the path
/// and the error, which is the same shape `sweep_orphan_transfer_dirs`
/// already uses for the same class of failure.
fn remove_staging_dir(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir) {
        tracing::warn!(
            path = %dir.display(),
            error = %error,
            "staging directory could not be removed; the orphan sweep at the next start reclaims it"
        );
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

fn load_write_markers_from_connection(conn: &Connection) -> Result<HashMap<String, SystemTime>> {
    let mut stmt = conn.prepare("SELECT name,written_at FROM write_markers")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut writes = HashMap::new();
    for row in rows {
        let (name, seconds) = row?;
        let Ok(seconds) = u64::try_from(seconds) else {
            continue;
        };
        if let Some(stamp) = UNIX_EPOCH.checked_add(Duration::from_secs(seconds)) {
            writes.insert(name, stamp);
        }
    }
    Ok(writes)
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
        // One unreadable row refuses startup (fail closed), but the log
        // names the row so the operator can fix or remove exactly it.
        if let Err(error) = migrate_one_scope(conn, &credential, &raw) {
            tracing::error!(
                credential = %credential,
                code = error.code,
                "credential metadata unreadable; refusing startup"
            );
            return Err(error);
        }
    }
    Ok(())
}

fn migrate_one_scope(conn: &Connection, credential: &str, raw: &str) -> Result<()> {
    let corrupt = Error::metadata;
    let value: Value = serde_json::from_str(raw).map_err(|_| corrupt())?;
    let entries = value.as_array().ok_or_else(corrupt)?;
    // New-format rows are already generation-bound. Parsing validates
    // their shape and rejects malformed metadata rather than broadening
    // it at startup.
    if !entries.iter().all(Value::is_string) {
        parse_scopes(raw).map_err(|_| corrupt())?;
        return Ok(());
    }
    let mut scopes = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.as_str().ok_or_else(corrupt)?;
        if validate_name(name).is_err() {
            return Err(corrupt());
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
                return Err(corrupt());
            }
            scopes.push(DatabaseScope {
                name: name.to_string(),
                generation,
            });
        }
    }
    let encoded = serde_json::to_string(&scopes).map_err(|_| corrupt())?;
    conn.execute(
        "UPDATE credentials SET databases=? WHERE name=?",
        params![encoded, credential],
    )?;
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

/// Manifests are kilobytes; anything larger is corruption or abuse, never
/// a real set. Both the metadata pre-check and the post-read length check
/// matter: the first avoids the allocation, the second closes the race.
pub(crate) fn read_manifest_capped(path: &Path) -> Option<Vec<u8>> {
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
    if std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0) > MAX_MANIFEST_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return None;
    }
    Some(bytes)
}

/// Only a uniqueness violation means the name is taken; every other
/// insert failure (I/O, corruption) keeps its underlying mapping instead
/// of masquerading as a duplicate name.
fn map_issue_error(error: rusqlite::Error) -> Error {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        Error::new(409, "key_exists", "credential name already exists")
    } else {
        error.into()
    }
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

/// fsync a directory so a name newly linked or created inside it survives a
/// crash. The data fsync on the file is not enough: the directory entry is a
/// separate write, and losing it after the registry row is durable leaves a
/// registered database with no file. Mirrors the CLI's publish_export_file.
fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| Error::internal())
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
    fn failed_commit_restores_open_only_while_the_ttl_is_live() {
        // A long-lived upload: a rejected target name must still restore
        // Open so the client can retry with a corrected name.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let live = store.upload_begin().unwrap();
        {
            let mut transfers = store.transfers.lock().unwrap();
            let TransferKind::Upload { state, .. } = &mut transfers.get_mut(&live.id).unwrap().kind
            else {
                panic!("upload kind")
            };
            *state = UploadState::Committing;
        }
        let mut transfers = store.transfers.lock().unwrap();
        store.restore_open_after_failed_commit(&live.id, &mut transfers);
        let TransferKind::Upload { state, .. } = &transfers.get(&live.id).unwrap().kind else {
            panic!("upload kind")
        };
        assert_eq!(
            *state,
            UploadState::Open,
            "a live upload must stay retryable"
        );

        // An expired upload: publication consumed the remaining TTL, so the
        // entry must NOT be revived. purge_expired_locked removes it, which
        // is what the old code allowed only by accident -- it set Open on a
        // row the very next append/abort would then 404 for.
        let short = Store::open_with_ttl(dir.path(), Duration::from_millis(0)).unwrap();
        let expired = short.upload_begin().unwrap();
        {
            let mut transfers = short.transfers.lock().unwrap();
            // created.elapsed() >= ttl (== 0) is true immediately, so the
            // entry is already expired at the moment publication returns.
            let TransferKind::Upload { state, .. } =
                &mut transfers.get_mut(&expired.id).unwrap().kind
            else {
                panic!("upload kind")
            };
            *state = UploadState::Committing;
        }
        let mut transfers = short.transfers.lock().unwrap();
        short.restore_open_after_failed_commit(&expired.id, &mut transfers);
        assert!(
            transfers.get(&expired.id).is_none(),
            "an upload whose TTL elapsed during publication must not be revived"
        );
    }

    #[test]
    fn write_marker_contention_never_reports_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("db").unwrap();
        store.begin_write("db").unwrap();

        let blocker = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.finish_write("db");
        assert!(store.last_write("db").is_none());
        drop(blocker);

        let reopened = Store::open(dir.path()).unwrap();
        assert!(reopened.last_write("db").is_none());
    }

    #[test]
    fn write_markers_survive_restart_and_older_malformed_set_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("db").unwrap();
        store.note_write_legacy("db").unwrap();
        let before = store.last_write("db").unwrap();
        drop(store);

        let reopened = Store::open(dir.path()).unwrap();
        let after = reopened.last_write("db").unwrap();
        assert!(after <= before && before.duration_since(after).unwrap().as_secs() < 1);

        let root = dir.path().join("sets");
        std::fs::create_dir_all(root.join("20200101T000000Z")).unwrap();
        std::fs::create_dir_all(root.join("20200101T000001Z")).unwrap();
        std::fs::write(root.join("20200101T000000Z/MANIFEST.json"), "{").unwrap();
        std::fs::write(
            root.join("20200101T000001Z/manifest.json"),
            r#"{"databases":[{"name":"db"}],"registry":{"file":"metadata.sqlite.bak","sha256":"x"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("20200101T000001Z/metadata.sqlite.bak"),
            b"registry",
        )
        .unwrap();
        let with_root = reopened.with_backup_root(Some(root));
        let sets = with_root.backup_sets().unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].timestamp, "20200101T000001Z");
    }

    #[test]
    fn small_page_export_and_clone_honor_lifecycle_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("small").unwrap();
        let path = store.path("small").unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=DELETE;
                 PRAGMA page_size=512;
                 VACUUM;
                 CREATE TABLE payload(value BLOB);
                 INSERT INTO payload VALUES (zeroblob(20000000));",
            )
            .unwrap();
        }
        let page_size: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .unwrap();
        assert_eq!(page_size, 512);
        let source_size = std::fs::metadata(&path).unwrap().len();
        assert!(source_size > 512 * 64 * 512);
        assert!(source_size < MAX_TRANSFER_SIZE);

        let snapshot = store.export_snapshot("small").unwrap();
        assert!(snapshot.path().metadata().unwrap().len() < MAX_TRANSFER_SIZE);
        drop(snapshot);
        store.clone_database("small", "small_copy").unwrap();
        assert!(store.path("small_copy").is_ok());
    }

    #[test]
    fn concurrent_writes_do_not_turn_a_valid_export_into_a_false_busy_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("live").unwrap();
        let path = store.path("live").unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, value BLOB);
                 INSERT INTO t(value) VALUES (zeroblob(2000000));",
            )
            .unwrap();
        }
        let writer_path = path.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_stop = stop.clone();
        let writer = std::thread::spawn(move || {
            let conn = Connection::open(writer_path).unwrap();
            let mut id = 1;
            while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = conn.execute("INSERT INTO t(value) VALUES (?1)", [id as i64]);
                id += 1;
            }
        });
        let snapshot = store.export_snapshot("live").unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert!(snapshot.path().metadata().unwrap().len() < MAX_TRANSFER_SIZE);
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
    fn live_wal_import_preserves_committed_sidecar_rows() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("live.sqlite");
        let source = Connection::open(&source_path).unwrap();
        source
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE t(value INTEGER);
                 INSERT INTO t VALUES (42);",
            )
            .unwrap();
        assert!(
            std::fs::metadata(dir.path().join("live.sqlite-wal"))
                .unwrap()
                .len()
                > 0
        );

        let store_root = dir.path().join("store");
        let store = Store::open(&store_root).unwrap();
        store.bootstrap().unwrap();
        store.import_from("restored", &source_path).unwrap();
        let restored = store.path("restored").unwrap();
        let value: i64 = Connection::open(restored)
            .unwrap()
            .query_row("SELECT value FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 42);
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
    fn corrupt_credential_row_refuses_startup_with_named_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
             CREATE TABLE databases(name TEXT PRIMARY KEY, generation TEXT);
             INSERT INTO credentials(name, hash, role, databases) VALUES ('badkey', 'h', 'owner', '{oops');",
        )
        .unwrap();
        let error = migrate_metadata(&mut conn, dir.path()).unwrap_err();
        assert_eq!(error.code, "metadata_corrupt");
    }

    #[test]
    fn revoke_and_delete_report_unknown_and_repeated_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.revoke("ghost").unwrap_err().code, "key_not_found");
        store.create("doomed").unwrap();
        store.delete("doomed", 3600).unwrap();
        assert_eq!(
            store.delete("doomed", 3600).unwrap_err().code,
            "already_deleted"
        );
    }

    #[test]
    fn issue_error_mapping_distinguishes_duplicates_from_failures() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("scoped").unwrap();
        let request = || KeyRequest {
            name: "app".to_string(),
            role: "database_read_write".to_string(),
            databases: vec!["scoped".to_string()],
        };
        store.issue(request()).unwrap();
        assert_eq!(store.issue(request()).unwrap_err().code, "key_exists");
        // A non-constraint failure keeps its own mapping instead of key_exists.
        let conn = Connection::open_in_memory().unwrap();
        let error = conn
            .execute("INSERT INTO no_such_table VALUES (1)", [])
            .unwrap_err();
        assert_eq!(map_issue_error(error).code, "invalid_sql");
    }

    #[test]
    fn snapshot_chunk_rejects_zero_length() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = store.bootstrap().unwrap();
        let principal = store.authenticate(&key).unwrap();
        store.create("snapdb").unwrap();
        let snap = store.export_begin(&principal, "snapdb").unwrap();
        assert_eq!(
            store
                .snapshot_chunk(&principal, &snap.id, 0, 0)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        assert!(store.snapshot_chunk(&principal, &snap.id, 0, 64).is_ok());
    }

    #[test]
    fn export_claims_destination_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("racedb").unwrap();
        let dest = dir.path().join("claimed.sqlite");
        store.export_to("racedb", &dest).unwrap();
        assert_eq!(
            store.export_to("racedb", &dest).unwrap_err().code,
            "file_exists"
        );
        // A pre-existing non-database file is also a 409, never an open
        // through a swapped path.
        let squat = dir.path().join("squat.sqlite");
        std::fs::write(&squat, b"not a database").unwrap();
        assert_eq!(
            store.export_to("racedb", &squat).unwrap_err().code,
            "file_exists"
        );
    }

    #[test]
    fn concurrent_export_same_destination_has_one_winner() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        store.create("conrsedb").unwrap();
        let dest = dir.path().join("shared.sqlite");
        let barrier = Arc::new(Barrier::new(2));
        let race = |store: Arc<Store>, barrier: Arc<Barrier>, dest: std::path::PathBuf| {
            barrier.wait();
            store
                .export_to_limited("conrsedb", &dest, None)
                .map(|()| "ok")
                .unwrap_or_else(|error| {
                    assert_eq!(error.code, "file_exists");
                    "conflict"
                })
        };
        let (first, second) = std::thread::scope(|scope| {
            let one = scope.spawn(|| race(Arc::clone(&store), Arc::clone(&barrier), dest.clone()));
            let two = scope.spawn(|| race(Arc::clone(&store), Arc::clone(&barrier), dest.clone()));
            (one.join().unwrap(), two.join().unwrap())
        });
        assert!(first != second, "one export must win, one must 409");
    }

    #[test]
    fn acquire_queue_records_waits_and_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("qdb").unwrap();
        // Concurrent readers admit each other immediately.
        let first = store.acquire_queue("qdb", false).unwrap();
        let _second = store.acquire_queue("qdb", false).unwrap();
        drop(first);
        let snapshot = store.metrics.queue_snapshot();
        assert_eq!(snapshot["qdb"].waits, 2);
        assert_eq!(snapshot["qdb"].timeouts, 0);
        // An exclusively held queue times the waiter out at the bound.
        drop(_second);
        let _exclusive = store.acquire_queue("qdb", true).unwrap();
        let error = match store.acquire_queue("qdb", false) {
            Ok(_) => panic!("exclusive hold must time the reader out"),
            Err(error) => error,
        };
        assert_eq!(error.code, "capacity_exceeded");
        let snapshot = store.metrics.queue_snapshot();
        // Four, not three: the writer that takes the exclusive guard now goes
        // through acquire_queue like every other admission, so it is counted
        // too. The previous count of three came from holding the raw
        // RwLock directly, which bypassed the metrics and so reported fewer
        // waits than the server actually served.
        assert_eq!(snapshot["qdb"].waits, 4);
        assert_eq!(snapshot["qdb"].timeouts, 1);
        assert!(snapshot["qdb"].wait_ms >= ACQUIRE_TIMEOUT.as_millis() as u64);
    }

    /// A read flood must not delay a writer. This is the Phase 2 soak
    /// measurement turned into a test: a writer queues behind a held reader
    /// batch, and a reader arriving after that writer must NOT jump in front
    /// of it.
    ///
    /// The setup is deliberately a held READ guard, not a held write guard.
    /// Holding the exclusive guard would block any arriving reader anyway and
    /// the test would pass with the queue ordering deleted -- it has to be a
    /// situation where a reader *could* get in and only the phase ordering
    /// stops it.
    #[test]
    fn waiting_writer_makes_new_readers_yield() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        store.create("qdb").unwrap();

        // A reader holding the shared guard: more readers may still join it,
        // but the writer below cannot take the exclusive guard until it
        // drops, so the writer has to queue. That is the state under test.
        let held = store.acquire_queue("qdb", false).unwrap();

        // One monotonic counter stamps admission order for both threads.
        let stamp = Arc::new(AtomicUsize::new(0));
        let writer_at = Arc::new(AtomicUsize::new(usize::MAX));
        let reader_at = Arc::new(AtomicUsize::new(usize::MAX));
        let writer_store = store.clone();
        let writer_slot = writer_at.clone();
        let writer_stamp = stamp.clone();
        let writer = thread::spawn(move || {
            let guard = writer_store.acquire_queue("qdb", true).unwrap();
            writer_slot.store(
                writer_stamp.fetch_add(1, AtomicOrdering::SeqCst),
                AtomicOrdering::SeqCst,
            );
            drop(guard);
        });

        // Wait for the writer to actually hold a ticket rather than assuming
        // a delay, but stay well inside ACQUIRE_TIMEOUT so it has not given
        // up.
        let queued = (0..200).any(|_| {
            if store
                .queue("qdb")
                .unwrap()
                .state
                .lock()
                .map(|state| !state.waiting_writers.is_empty())
                .unwrap_or(false)
            {
                true
            } else {
                thread::sleep(Duration::from_millis(2));
                false
            }
        });
        assert!(queued, "writer must take a ticket before it waits");

        let reader_store = store.clone();
        let reader_slot = reader_at.clone();
        let reader_stamp = stamp.clone();
        let reader = thread::spawn(move || {
            let guard = reader_store.acquire_queue("qdb", false).unwrap();
            reader_slot.store(
                reader_stamp.fetch_add(1, AtomicOrdering::SeqCst),
                AtomicOrdering::SeqCst,
            );
            drop(guard);
        });

        // Generous: an unbounded reader would certainly have taken the shared
        // guard by now.
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            reader_at.load(AtomicOrdering::SeqCst),
            usize::MAX,
            "a reader jumped in front of a queued writer"
        );

        // Now the part that actually matters, and the part the previous
        // version of this test never reached: it dropped the guard and then
        // only asserted that BOTH threads eventually got in, which holds in
        // either order. The reader is already queued, so the batch boundary
        // is the only thing standing between it and the exclusive guard.
        //
        // This has to assert an ORDER, not a flag read after the fact: the
        // writer releases the guard the moment it is admitted, so checking
        // "the reader has not been admitted yet" after the writer thread
        // joins races the reader's admission and fails intermittently.
        // Admission order is recorded from a single monotonic counter instead,
        // so the claim is exactly the one that matters: of the two, the
        // writer went first.
        drop(held);
        writer.join().unwrap();
        reader.join().unwrap();
        let (writer_at, reader_at) = (
            writer_at.load(AtomicOrdering::SeqCst),
            reader_at.load(AtomicOrdering::SeqCst),
        );
        assert!(
            writer_at < reader_at,
            "a reader arriving after a queued writer took the batch boundary \
             (writer admitted #{writer_at}, reader #{reader_at})"
        );
    }

    /// The regression the writer-preferring FLAG caused, which is why the
    /// queue is a fair ticket lock rather than a flag: that flag fixed the
    /// writers
    /// and starved the readers, 4,260 reader rejections against 5 writer
    /// rejections in one soak run. A continuous stream of writers must still
    /// let a waiting reader in.
    #[test]
    fn continuous_writers_do_not_starve_a_waiting_reader() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        store.create("qdb").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        // WRITE_PERMITS writers, each holding the exclusive guard long enough
        // to keep a real backlog queued. With too few writers, or ones that
        // release instantly, the queue drains between acquisitions and a
        // reader slips through a gap -- which would make this test pass even
        // when readers ARE being starved.
        for _ in 0..4 {
            let writer_store = store.clone();
            let writer_stop = stop.clone();
            writers.push(thread::spawn(move || {
                while !writer_stop.load(AtomicOrdering::SeqCst) {
                    if let Ok(guard) = writer_store.acquire_queue("qdb", true) {
                        for _ in 0..20_000 {
                            std::hint::spin_loop();
                        }
                        drop(guard);
                    }
                }
            }));
        }

        // Let the writers establish a backlog, then require that a reader
        // arriving into the middle of it still gets admitted.
        thread::sleep(Duration::from_millis(40));
        let reader = store.acquire_queue("qdb", false);
        stop.store(true, AtomicOrdering::SeqCst);
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(
            reader.is_ok(),
            "a waiting reader was starved by a continuous writer stream"
        );
    }

    /// A SUSTAINED read flood must not deny writers. The reader threads here
    /// re-acquire immediately with no client-side gap, which is the shape a
    /// genuinely read-bound workload has.
    ///
    /// This asserts a RATE, so read it as a property check on the design in
    /// front of you and not as a regression guard: the replaced phase design
    /// passes it, because there a writer was admitted whenever one of its
    /// threads happened to observe `readers_active == 0`, and how often that
    /// happened was a property of the harness rather than the queue. The
    /// guard against that design is the ORDER assertion in
    /// `admission_follows_queue_order_not_a_race`.
    ///
    /// Reader count matches the server's own READ_PERMITS=8.
    #[test]
    fn sustained_read_flood_still_admits_writers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        store.create("qdb").unwrap();
        for hold in [Duration::from_millis(2), Duration::from_millis(20)] {
            sustained_flood_case(&store, hold);
        }
    }

    /// One sustained-flood case at a given reader hold time. 2ms is the fast
    /// case; 20ms is the slow one, and it is the one that matters most: a
    /// writer's wait is bounded by the batch of readers already inside, so a
    /// longer hold is a longer wait, and 8 readers x 20ms = 160ms is still
    /// inside ACQUIRE_TIMEOUT. A hold long enough to push a writer past the
    /// bound would be a genuinely unreadable read, not a realistic one.
    fn sustained_flood_case(store: &Arc<Store>, hold: Duration) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
        const READERS: usize = 8;
        const WRITERS: usize = 2;
        let stop = Arc::new(AtomicBool::new(false));
        let granted = Arc::new(AtomicUsize::new(0));
        let rejected = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..READERS {
            let s = store.clone();
            let t = stop.clone();
            threads.push(thread::spawn(move || {
                while !t.load(AtomicOrdering::SeqCst) {
                    if let Ok(_g) = s.acquire_queue("qdb", false) {
                        // A real hold, like a range scan over a few thousand
                        // rows, then straight back in with no client gap for a
                        // writer to slip through.
                        thread::sleep(hold);
                    }
                }
            }));
        }
        for _ in 0..WRITERS {
            let s = store.clone();
            let t = stop.clone();
            let g = granted.clone();
            let r = rejected.clone();
            threads.push(thread::spawn(move || {
                while !t.load(AtomicOrdering::SeqCst) {
                    match s.acquire_queue("qdb", true) {
                        Ok(_g) => g.fetch_add(1, AtomicOrdering::SeqCst),
                        Err(_) => r.fetch_add(1, AtomicOrdering::SeqCst),
                    };
                }
            }));
        }

        thread::sleep(Duration::from_millis(1_000));
        stop.store(true, AtomicOrdering::SeqCst);
        for t in threads {
            t.join().unwrap();
        }
        let ok = granted.load(AtomicOrdering::SeqCst);
        let bad = rejected.load(AtomicOrdering::SeqCst);
        let total = (ok + bad).max(1);
        let rate = ok as f64 / total as f64;
        assert!(
            rate > 0.25,
            "sustained read flood with a {hold:?} hold denied writers: only \
             {ok}/{total} admitted ({:.1}%), {bad} timed out at ACQUIRE_TIMEOUT",
            rate * 100.0
        );
    }

    /// The exclusive guard must actually exclude. A ticket lock is only worth
    /// its complexity if it never lets a writer in beside a reader or beside
    /// another writer, and nothing else in the suite asserted that directly:
    /// `readers_still_share_the_guard_when_no_writer_waits` covers only the
    /// reader-sharing case. The counters are bumped INSIDE the guard, so any
    /// overlap the queue wrongly allowed shows up as a violation.
    #[test]
    fn the_exclusive_guard_excludes_readers_and_other_writers() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as Seq};
        const WRITERS: usize = 4;
        const READERS: usize = 4;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        store.create("qdb").unwrap();

        let active_writers = Arc::new(AtomicUsize::new(0));
        let active_readers = Arc::new(AtomicUsize::new(0));
        let two_writers = Arc::new(AtomicUsize::new(0));
        let writer_beside_reader = Arc::new(AtomicUsize::new(0));
        let writer_admits = Arc::new(AtomicUsize::new(0));
        let reader_admits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        for _ in 0..WRITERS {
            let s = store.clone();
            let aw = active_writers.clone();
            let ar = active_readers.clone();
            let two = two_writers.clone();
            let beside = writer_beside_reader.clone();
            let ok = writer_admits.clone();
            let t = stop.clone();
            threads.push(thread::spawn(move || {
                while !t.load(Seq::SeqCst) {
                    let Ok(_guard) = s.acquire_queue("qdb", true) else {
                        continue;
                    };
                    ok.fetch_add(1, Seq::SeqCst);
                    if aw.fetch_add(1, Seq::SeqCst) != 0 {
                        two.fetch_add(1, Seq::SeqCst);
                    }
                    if ar.load(Seq::SeqCst) != 0 {
                        beside.fetch_add(1, Seq::SeqCst);
                    }
                    for _ in 0..2_000 {
                        std::hint::spin_loop();
                    }
                    aw.fetch_sub(1, Seq::SeqCst);
                }
            }));
        }
        for _ in 0..READERS {
            let s = store.clone();
            let aw = active_writers.clone();
            let ar = active_readers.clone();
            let beside = writer_beside_reader.clone();
            let ok = reader_admits.clone();
            let t = stop.clone();
            threads.push(thread::spawn(move || {
                while !t.load(Seq::SeqCst) {
                    let Ok(_guard) = s.acquire_queue("qdb", false) else {
                        continue;
                    };
                    ok.fetch_add(1, Seq::SeqCst);
                    // Checked before this reader is counted, so the only way
                    // to see a writer here is a genuine overlap.
                    if aw.load(Seq::SeqCst) != 0 {
                        beside.fetch_add(1, Seq::SeqCst);
                    }
                    ar.fetch_add(1, Seq::SeqCst);
                    for _ in 0..2_000 {
                        std::hint::spin_loop();
                    }
                    ar.fetch_sub(1, Seq::SeqCst);
                }
            }));
        }

        thread::sleep(Duration::from_millis(800));
        stop.store(true, Seq::SeqCst);
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            two_writers.load(Seq::SeqCst),
            0,
            "two writers held the exclusive guard at once"
        );
        assert_eq!(
            writer_beside_reader.load(Seq::SeqCst),
            0,
            "a writer was admitted while a reader held the shared guard"
        );
        // Non-vacuity: both lanes have to have actually run, or the two
        // assertions above prove nothing.
        assert!(
            writer_admits.load(Seq::SeqCst) > 0 && reader_admits.load(Seq::SeqCst) > 0,
            "nothing was admitted, so the exclusion proved nothing: {} writer \
             admissions, {} reader admissions",
            writer_admits.load(Seq::SeqCst),
            reader_admits.load(Seq::SeqCst)
        );
    }

    /// Admission must follow QUEUE ORDER, not thread scheduling. This is the
    /// guard that actually protects the design, and it is deliberately an
    /// order assertion rather than an admission rate.
    ///
    /// The rate assertion in `sustained_read_flood_still_admits_writers` was
    /// tried first and is a property check, not a regression guard: the
    /// replaced phase design passes it, because in that design a writer was
    /// admitted whenever a writer thread happened to observe
    /// `readers_active == 0`, which a busy harness often lets it do. Its
    /// observed rate swung from 0/48 to ~11,292/~48 depending only on thread
    /// count and pacing.
    ///
    /// Order has no such freedom. Here a writer is queued first, so its
    /// ticket is smaller, and `serving` only advances in ticket order -- a
    /// reader CANNOT be admitted first, on any schedule.
    ///
    /// The number of readers behind that writer is the whole test. One
    /// reader is NOT enough to catch the old design: a condvar wakes waiters
    /// in the order they blocked, and a single queued reader therefore loses
    /// the state mutex to the writer it overtook, so the replaced phase
    /// design passes a one-reader version of this test (measured: 0 of 300
    /// trials lost, against 0 of 300 for this one). What defeats that design
    /// is a BATCH, because it re-opens the read phase to every queued reader
    /// at each batch boundary and the winner is whichever of them the
    /// scheduler runs first -- not the writer holding the older ticket. So
    /// this queues READERS_BEHIND the writer, matching the server's own
    /// READ_PERMITS, and counts a trial lost if ANY of them is admitted
    /// first. Run against the replaced design (a96f261, patched into a
    /// scratch copy of this crate) it fails every run -- 65, 135 and 67
    /// trials lost per 100, at load1 78-80. The rate is a race rate, so it
    /// moves with machine load; the zero on this design is structural rather
    /// than lucky. `scripts/admission_order_probe.sh` rebuilds both queues
    /// verbatim and reproduces the contrast without patching a crate.
    #[test]
    fn admission_follows_queue_order_not_a_race() {
        use std::sync::atomic::{AtomicUsize, Ordering as Seq};
        const TRIALS: usize = 100;
        /// The server's READ_PERMITS: a reader batch this size is what a
        /// read flood actually looks like, and it is the size the old
        /// design's phase re-open hands the queue to.
        const READERS_BEHIND: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        store.create("qdb").unwrap();
        let mut lost_trials = 0;
        let mut overtakings = 0;

        for trial in 0..TRIALS {
            // A guard already held, so the writer and every reader queue
            // behind it and their relative order is decided by when they take
            // their tickets.
            let held = store.acquire_queue("qdb", false).unwrap();
            let stamp = Arc::new(AtomicUsize::new(0));
            let writer_at = Arc::new(AtomicUsize::new(usize::MAX));

            let ws = store.clone();
            let wst = stamp.clone();
            let wat = writer_at.clone();
            let writer = thread::spawn(move || {
                let Ok(guard) = ws.acquire_queue("qdb", true) else {
                    return;
                };
                wat.store(wst.fetch_add(1, Seq::SeqCst), Seq::SeqCst);
                drop(guard);
            });
            // Only queue the reader once the writer really holds a ticket, so
            // "first to queue" is not itself a race.
            let queued = (0..500).any(|_| {
                if store
                    .queue("qdb")
                    .unwrap()
                    .state
                    .lock()
                    .map(|state| !state.waiting_writers.is_empty())
                    .unwrap_or(false)
                {
                    true
                } else {
                    thread::sleep(Duration::from_millis(1));
                    false
                }
            });
            assert!(queued, "trial {trial}: writer never queued");

            let mut readers = Vec::new();
            for _ in 0..READERS_BEHIND {
                let rs = store.clone();
                let rst = stamp.clone();
                let rat = Arc::new(AtomicUsize::new(usize::MAX));
                let stamped = rat.clone();
                readers.push((
                    rat,
                    thread::spawn(move || {
                        let Ok(guard) = rs.acquire_queue("qdb", false) else {
                            return;
                        };
                        stamped.store(rst.fetch_add(1, Seq::SeqCst), Seq::SeqCst);
                        drop(guard);
                    }),
                ));
            }

            drop(held);
            writer.join().unwrap();
            let writer_slot = writer_at.load(Seq::SeqCst);
            let mut first_ahead = 0;
            for (at, reader) in readers {
                reader.join().unwrap();
                // Every reader here queued AFTER the writer, so any of them
                // stamped ahead of it is an overtaking.
                if at.load(Seq::SeqCst) < writer_slot {
                    first_ahead += 1;
                }
            }
            overtakings += first_ahead;
            lost_trials += usize::from(first_ahead > 0);
        }
        assert_eq!(
            lost_trials, 0,
            "a reader was admitted ahead of a writer that queued first, in \
             {lost_trials} of {TRIALS} trials with {READERS_BEHIND} readers \
             queued behind it ({overtakings} overtakings in total): admission \
             is being decided by scheduling instead of by queue order"
        );
    }

    /// The bound must not simply hand the queue to the writers: with no
    /// writer waiting, concurrent readers still admit each other at once.
    #[test]
    fn readers_still_share_the_guard_when_no_writer_waits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("qdb").unwrap();
        let first = store.acquire_queue("qdb", false).unwrap();
        let second = store.acquire_queue("qdb", false).unwrap();
        drop(first);
        drop(second);
    }

    #[test]
    fn snapshot_chunk_after_release_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = store.bootstrap().unwrap();
        let principal = store.authenticate(&key).unwrap();
        store.create("reldb").unwrap();
        let snap = store.export_begin(&principal, "reldb").unwrap();
        assert!(store.snapshot_chunk(&principal, &snap.id, 0, 64).is_ok());
        store.snapshot_release(&principal, &snap.id).unwrap();
        assert_eq!(
            store
                .snapshot_chunk(&principal, &snap.id, 0, 64)
                .unwrap_err()
                .code,
            "snapshot_not_found"
        );
    }

    #[test]
    fn info_counts_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("big").unwrap();
        let path = store.path("big").unwrap();
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER)").unwrap();
        {
            let tx = conn.transaction().unwrap();
            for i in 0..5000 {
                tx.execute("INSERT INTO t VALUES (?)", [i]).unwrap();
            }
            tx.commit().unwrap();
        }
        drop(conn);
        let info = store.info("big").unwrap();
        assert_eq!(
            info["tables"],
            serde_json::json!([{"name": "t", "rows": 5000}])
        );
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

    #[test]
    fn purge_prunes_per_name_state_and_write_markers() {
        // Per-name state is keyed by database name, so a create/purge cycle on
        // fresh names used to leave the queue guard, the usage counters, the
        // queue-wait counters, and the durable write marker behind forever --
        // three in-memory maps growing without bound over the process
        // lifetime, plus a metadata table that reloads every row at boot.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        for index in 0..4 {
            let name = format!("cycle{index}");
            store.create(&name).unwrap();
            // Touch every per-name map the way a served request would.
            let _guard = store.acquire_queue(&name, false).unwrap();
            store.metrics.record_usage(&name, 10, 20);
            store
                .metrics
                .record_queue_wait(&name, Duration::from_millis(3), false);
            assert!(store.queues.lock().unwrap().contains_key(&name));
            assert!(store.metrics.usage_snapshot().contains_key(&name));
            assert!(store.metrics.queue_snapshot().contains_key(&name));
            let markers: i64 = store
                .meta()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM write_markers WHERE name=?",
                    [&name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(markers, 1, "create must leave a freshness marker");

            store.delete(&name, 0).unwrap();
            assert_eq!(store.purge_expired().unwrap(), vec![name.clone()]);

            assert!(!store.queues.lock().unwrap().contains_key(&name));
            assert!(!store.metrics.usage_snapshot().contains_key(&name));
            assert!(!store.metrics.queue_snapshot().contains_key(&name));
            let markers: i64 = store
                .meta()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM write_markers WHERE name=?",
                    [&name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(markers, 0, "purge must drop the freshness marker");
        }
        assert!(
            store.queues.lock().unwrap().is_empty(),
            "the queue map must not grow across create/purge cycles"
        );
    }

    /// A staging directory that cannot be removed must be reported, not
    /// silently dropped.
    ///
    /// The defect this pins: every transfer-cleanup path used
    /// `let _ = std::fs::remove_dir_all(&dir)`, so a transient EACCES/EBUSY
    /// (a stale NFS handle, a transient LSM lock, an antivirus scan of a
    /// large upload) removed the in-memory transfer entry while leaving the
    /// directory on disk — up to 128 MiB per transfer, `MAX_TRANSFERS` = 8 —
    /// with no log line and no path back to the data. The directory became
    /// unreachable until the orphan sweep at the next start, which counts
    /// nothing and reports nothing until it runs.
    ///
    /// The test builds exactly that state: a transfer whose parent directory
    /// is made read-only so `remove_dir_all` fails, then asserts that (a) the
    /// abort still succeeds — a failed cleanup must not fail the request that
    /// asked for the removal, because the data is already unreachable — and
    /// (b) the entry is gone from the map, so the invariant "the entry is
    /// removed even when the directory is not" is explicit rather than
    /// incidental, and the leak is at least reachable by the orphan sweep.
    #[test]
    fn unremovable_staging_dir_still_completes_the_abort() {
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
        // The stage lives directly in the data root, so making the root
        // read-only makes the stage unremovable while leaving the data root
        // itself writable enough for the rest of the store to work.
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(mode & !0o222),
        )
        .unwrap();
        // With the parent unwritable the cleanup cannot succeed; the code
        // path under test must not let that fail the caller.
        let aborted = store.upload_abort(&upload.id);
        // Restore before asserting so the TempDir can still clean up.
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )
        .unwrap();
        assert!(
            aborted.is_ok(),
            "an unremovable staging directory must not fail the abort: {:?}",
            aborted
        );
        assert!(
            transfer_dir.exists(),
            "the stage is expected to survive: the test made its parent unwritable"
        );
        // The in-memory entry is gone, which is what makes the orphan sweep
        // the (only) reclaim path — and what makes a silent failure a real
        // leak. Restoring the mode lets the sweep reclaim it.
        assert!(store.transfers.lock().unwrap().is_empty());
        let sweep_ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sweep_orphan_transfer_dirs(dir.path());
        }));
        assert!(sweep_ran.is_ok(), "the orphan sweep must stay runnable");
        assert!(
            !transfer_dir.exists(),
            "once the filesystem obstacle clears, the orphan sweep must reclaim the stage"
        );
    }
}
