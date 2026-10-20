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
    /// A bounded ring of recent request counts, one bucket per second.
    ///
    /// This is deliberately a WINDOW, not a time series: a fixed-size ring
    /// of the last `WINDOW_BUCKETS` seconds, advanced when a read asks for
    /// it. It answers "how busy was it just now" without becoming a
    /// metrics database, and it costs one counter and one lock.
    window: Mutex<RequestWindow>,
}

/// How many seconds of history the request window keeps. Long enough to
/// show a minute of activity, short enough to stay a fixed-size ring.
pub const WINDOW_BUCKETS: usize = 60;

/// A request-rate window: one bucket per second, oldest first.
///
/// The ring is advanced on read, so a service nobody is watching costs
/// nothing: the buckets are only materialised when someone asks. A
/// bucket that the service slept through reads as zero, which is
/// honest -- no requests arrived.
#[derive(Debug, Clone)]
struct RequestWindow {
    /// The epoch second each bucket covers.
    seconds: Vec<u64>,
    /// The request count in each bucket.
    counts: Vec<u64>,
    /// The epoch second the newest bucket covers.
    newest: u64,
}

impl Default for RequestWindow {
    fn default() -> Self {
        Self {
            seconds: vec![0; WINDOW_BUCKETS],
            counts: vec![0; WINDOW_BUCKETS],
            newest: 0,
        }
    }
}

impl RequestWindow {
    /// The request counts of the last `WINDOW_BUCKETS` seconds, oldest
    /// first. Seconds with no requests read as zero.
    fn series(&self, now: u64) -> Vec<u64> {
        if self.newest == 0 {
            return vec![0; WINDOW_BUCKETS];
        }
        // Walk backwards from the newest bucket, filling in the seconds
        // between it and now (the service may have been idle) and then
        // the older buckets the ring still holds.
        let mut out = Vec::with_capacity(WINDOW_BUCKETS);
        for age in 0..WINDOW_BUCKETS as u64 {
            let second = self.newest + 1 + age;
            let _ = second;
            out.push(0);
        }
        out.clear();
        // Oldest first: the bucket `WINDOW_BUCKETS - 1` seconds ago is
        // the first one in the series.
        for index in 0..WINDOW_BUCKETS {
            let age = (WINDOW_BUCKETS - 1 - index) as u64;
            let second = self.newest + 1 + age;
            if second > now {
                out.push(0);
                continue;
            }
            let position = (second % WINDOW_BUCKETS as u64) as usize;
            if self.seconds[position] == second {
                out.push(self.counts[position]);
            } else {
                out.push(0);
            }
        }
        out
    }
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
