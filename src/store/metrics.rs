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
    /// Per-database admission state: the counters plus the bounded ring
    /// of that database's own recent waits, so the per-database
    /// percentile is the DATABASE's rather than the service's.
    queue: Mutex<HashMap<String, DbQueueLive>>,
    /// A bounded ring of per-second request counts.
    ///
    /// This is deliberately a WINDOW, not a time series: a fixed-size
    /// ring of the last `WINDOW_BUCKETS` seconds. It answers "how busy
    /// was it just now" without becoming a metrics database, and it
    /// costs one atomic add per request.
    window: Box<[AtomicU64]>,
    /// The statement shapes the service has run, and what they cost.
    ///
    /// It is a separate structure from the counters because it answers a
    /// different question: not "how much has this database done" but
    /// "which of its statements is the expensive one".
    pub queries: QueryLog,
    /// The most recent admission-wait durations, for percentiles.
    ///
    /// A ring rather than a growing list: the percentile of the last
    /// few thousand waits is what an operator can act on, and the memory
    /// is fixed. Only the contended path writes here.
    waits: Mutex<Samples>,
    /// The most recent statement-execution durations, for percentiles.
    ///
    /// Waits answer "how contended is it"; this answers "how slow is
    /// it". Same ring, same bound, written once per executed statement
    /// next to the per-shape cost log.
    latency: Mutex<Samples>,
}

/// How many seconds of history the request window keeps.
pub const WINDOW_BUCKETS: usize = 60;

/// How many recent waits the service-wide percentile sample keeps.
pub const WAIT_SAMPLES: usize = 1024;

/// How many recent waits each database's own percentile sample keeps.
///
/// Smaller than the service-wide ring because this one is paid for
/// PER DATABASE: at the registry's cap that is a few hundred kilobytes
/// of samples, not a few. A percentile over a small sample is still a
/// percentile; it just speaks about a shorter recent past.
pub const DB_WAIT_SAMPLES: usize = 64;

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

/// One database's live admission state: the counters, plus the bounded
/// ring of that database's own recent waits.
///
/// The ring is per database on purpose. A service-wide percentile
/// answers "how contended is the service"; a per-database percentile
/// answers "which database is contended", and only the second can name
/// one. The global ring in `Metrics` answers the first.
#[derive(Debug)]
struct DbQueueLive {
    queue: DbQueue,
    samples: Samples,
    latency: Samples,
}

impl Default for DbQueueLive {
    fn default() -> Self {
        Self {
            queue: DbQueue::default(),
            samples: Samples::with_capacity(DB_WAIT_SAMPLES),
            latency: Samples::with_capacity(DB_WAIT_SAMPLES),
        }
    }
}

impl DbQueueLive {
    /// The served snapshot: the counters, plus this database's own
    /// percentile of its own recent waits and executions.
    fn snapshot(&self) -> DbQueue {
        DbQueue {
            wait_p95_us: self.samples.percentile(95.0),
            query_p95_us: self.latency.percentile(95.0),
            ..self.queue
        }
    }
}

/// A bounded ring of recent durations, in MICROSECONDS.
///
/// The percentile of the recent past is what an operator acts on ("p95
/// wait is 40ms"); the average over the whole uptime is not, because one
/// bad hour would define it forever. One ring shape serves both uses —
/// admission waits and statement latencies — because both are "how long
/// did the recent ones take" over a bounded recent past.
#[derive(Debug)]
struct Samples {
    values: Vec<u64>,
    next: usize,
    /// How many samples the ring keeps. The service-wide ring and each
    /// database's own ring are bounded differently, so the bound is a
    /// property of the ring rather than a constant.
    capacity: usize,
}

impl Default for Samples {
    fn default() -> Self {
        Self::with_capacity(WAIT_SAMPLES)
    }
}

impl Samples {
    /// A ring that keeps at most `capacity` samples.
    fn with_capacity(capacity: usize) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
            next: 0,
            capacity,
        }
    }

    /// Record a duration, keeping the ring bounded.
    fn push(&mut self, micros: u64) {
        if self.values.len() < self.capacity {
            self.values.push(micros);
            return;
        }
        self.values[self.next] = micros;
        self.next = (self.next + 1) % self.capacity;
    }

    /// The p-th percentile of the recorded durations, in microseconds.
    ///
    /// `None` when nothing has been recorded: a percentile of nothing
    /// is nothing, and reporting zero would claim the queue never
    /// contended or no statement ever ran.
    fn percentile(&self, p: f64) -> Option<u64> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = self.values.clone();
        sorted.sort_unstable();
        // The nearest-rank method: the smallest value at or above which
        // p percent of the samples fall.
        let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
        let index = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(sorted[index])
    }
}

/// Point-in-time copy for `/v1/status`. Plain data, no locks held.
#[derive(Debug)]
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub total: u64,
    pub capacity_exceeded: u64,
    pub deadline_exceeded: u64,
    pub internal_error: u64,
    /// The per-second request counts of the window, oldest first.
    pub request_window: Vec<u64>,
    /// How many seconds the window actually observed. Less than the
    /// window's length right after boot, so a rate is scaled honestly
    /// rather than reported against a span the service never lived.
    pub window_span_secs: u64,
    /// The median and 95th percentile of the recent admission waits, in
    /// MICROSECONDS. `None` when no wait has been recorded.
    pub wait_p50_us: Option<u64>,
    pub wait_p95_us: Option<u64>,
    /// How many waits the percentile sample holds.
    pub wait_samples: usize,
    /// The median and 95th percentile of recent statement-execution
    /// times, in MICROSECONDS. `None` when no statement has run.
    pub query_p50_us: Option<u64>,
    pub query_p95_us: Option<u64>,
    /// How many executions the percentile sample holds.
    pub query_samples: usize,
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
            window: (0..WINDOW_BUCKETS)
                .map(|_| AtomicU64::new(WINDOW_EMPTY))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            waits: Mutex::new(Samples::default()),
            latency: Mutex::new(Samples::default()),
            queries: QueryLog::new(),
        }
    }
    pub fn record_queue_wait(&self, name: &str, waited: Duration, timed_out: bool) {
        let micros = waited.as_micros() as u64;
        let millis = waited.as_millis() as u64;
        if let Ok(mut queue) = self.queue.lock() {
            let entry = queue.entry(name.to_string()).or_default();
            entry.queue.waits += 1;
            entry.queue.wait_ms += millis;
            entry.queue.max_wait_us = entry.queue.max_wait_us.max(micros);
            if timed_out {
                entry.queue.timeouts += 1;
            }
            // The database's own ring, so its percentile answers "how
            // bad is THIS database's queue" rather than "how bad is the
            // queue everywhere".
            entry.samples.push(micros);
        }
        // Only the contended path reaches here, so the ring's lock is
        // not on the request hot path.
        if let Ok(mut samples) = self.waits.lock() {
            samples.push(micros);
        }
    }
    /// Record one statement's execution time, service-wide and for its
    /// own database. Called next to the per-shape cost log, for
    /// executed statements only: a refused or timed-out statement has
    /// no execution time, and recording one would invent latency the
    /// database never spent.
    pub fn record_latency(&self, name: &str, micros: u64) {
        if let Ok(mut queue) = self.queue.lock() {
            queue
                .entry(name.to_string())
                .or_default()
                .latency
                .push(micros);
        }
        if let Ok(mut samples) = self.latency.lock() {
            samples.push(micros);
        }
    }
    pub fn queue_snapshot(&self) -> HashMap<String, DbQueue> {
        self.queue
            .lock()
            .map(|queue| {
                queue
                    .iter()
                    .map(|(name, live)| (name.clone(), live.snapshot()))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn record_usage(&self, name: &str, rows_read: u64, rows_written: u64) {
        if let Ok(mut usage) = self.usage.lock() {
            let entry = usage.entry(name.to_string()).or_default();
            entry.rows_read += rows_read;
            entry.rows_written += rows_written;
        }
    }
    /// Drop the per-name counters for a name with no live registry row.
    ///
    /// This is a BEST-EFFORT cleanup, not an exclusive operation. The
    /// caller is a request that holds the lifecycle lease in its SHARED
    /// form, so a concurrent request for the same name can still be
    /// recording into this entry -- and it can re-create the entry
    /// immediately after it was removed.
    ///
    /// That is why the entry is a diagnostic view and not a source of
    /// truth: a name whose row reappears in `/v1/status` is a name
    /// something touched recently, and the next failed write for it --
    /// or its purge -- clears it again. Taking the exclusive lease here
    /// would deadlock, because every admitted request already holds the
    /// shared one.
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
        self.record_window();
    }
    /// Count this request in the current second of the window.
    ///
    /// The hot path is one load and one add. Only when the second rolls
    /// over does the slot have to be claimed, which happens once a
    /// second per slot, not once a request.
    fn record_window(&self) {
        // Seconds since boot: monotonic, so a clock adjustment cannot
        // make the window go backwards.
        let now = self.started.elapsed().as_secs();
        let slot = (now % WINDOW_BUCKETS as u64) as usize;
        let current = self.window[slot].load(Ordering::Acquire);
        if current != WINDOW_EMPTY && (current >> WINDOW_COUNT_BITS) == now {
            // Same second: the count lives in the low half, and adding
            // one cannot carry into the second until the count reaches
            // 2^32 requests in one second.
            self.window[slot].fetch_add(1, Ordering::AcqRel);
            return;
        }
        // The second rolled over (or the slot is empty): claim it. A
        // request that lands on the boundary may be attributed to the
        // new second, which moves the rate by one request.
        let claimed = (now << WINDOW_COUNT_BITS) | 1;
        let mut current = current;
        loop {
            if current != WINDOW_EMPTY && (current >> WINDOW_COUNT_BITS) == now {
                self.window[slot].fetch_add(1, Ordering::AcqRel);
                return;
            }
            if current != WINDOW_EMPTY && (current >> WINDOW_COUNT_BITS) > now {
                // The slot holds a later second: the clock went
                // backwards, or another thread already claimed it.
                return;
            }
            match self.window[slot].compare_exchange_weak(
                current,
                claimed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
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
        let uptime_secs = self.started.elapsed().as_secs();
        let now = uptime_secs;
        // The window, oldest first. A slot holds its second in the high
        // half, so a stale slot is simply not counted, and an empty
        // slot is not mistaken for the first second of the service's
        // life.
        let mut request_window = vec![0u64; WINDOW_BUCKETS];
        for slot in 0..WINDOW_BUCKETS {
            let packed = self.window[slot].load(load);
            if packed == WINDOW_EMPTY {
                continue;
            }
            let second = packed >> WINDOW_COUNT_BITS;
            if second > now {
                // A later second: the clock went backwards.
                continue;
            }
            let age = now - second;
            if age >= WINDOW_BUCKETS as u64 {
                // Outside the window.
                continue;
            }
            request_window[WINDOW_BUCKETS - 1 - age as usize] = packed & WINDOW_COUNT_MASK;
        }
        // The window only observed the service's whole life once it is
        // older than the window itself.
        let window_span_secs = uptime_secs.min(WINDOW_BUCKETS as u64).max(1);
        let (wait_p50_us, wait_p95_us, wait_samples) = match self.waits.lock() {
            Ok(samples) => (
                samples.percentile(50.0),
                samples.percentile(95.0),
                samples.values.len(),
            ),
            Err(_) => (None, None, 0),
        };
        let (query_p50_us, query_p95_us, query_samples) = match self.latency.lock() {
            Ok(samples) => (
                samples.percentile(50.0),
                samples.percentile(95.0),
                samples.values.len(),
            ),
            Err(_) => (None, None, 0),
        };
        MetricsSnapshot {
            uptime_secs,
            total: self.total.load(load),
            capacity_exceeded: self.capacity_exceeded.load(load),
            deadline_exceeded: self.deadline_exceeded.load(load),
            internal_error: self.internal_error.load(load),
            request_window,
            window_span_secs,
            wait_p50_us,
            wait_p95_us,
            wait_samples,
            query_p50_us,
            query_p95_us,
            query_samples,
        }
    }
}

/// The high half of a window slot holds the second it counts; the low
/// half holds the count itself.
const WINDOW_COUNT_BITS: u32 = 32;
const WINDOW_COUNT_MASK: u64 = (1u64 << WINDOW_COUNT_BITS) - 1;

/// A slot that has never been claimed. It must be distinct from a
/// claimed slot, because "second 0 with no requests" and "no second at
/// all" are different facts.
const WINDOW_EMPTY: u64 = u64::MAX;
