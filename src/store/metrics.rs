use super::*;

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

/// Point-in-time copy for `/v1/status`. Plain data, no locks held.
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub total: u64,
    pub capacity_exceeded: u64,
    pub deadline_exceeded: u64,
    pub internal_error: u64,
}

impl Metrics {
    pub(super) fn new() -> Self {
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
