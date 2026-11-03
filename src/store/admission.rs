use super::*;

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
pub(super) struct QueueState {
    /// Readers currently holding the shared guard. Any number may hold it at
    /// once; the queue bounds the WRITER, not reader concurrency.
    readers_active: usize,
    writer_active: bool,
    /// Next ticket to hand out, and the ticket allowed to proceed next.
    next_ticket: u64,
    serving: u64,
    /// Tickets held by writers not yet admitted. A reader is held back by
    /// the earliest of these.
    pub(super) waiting_writers: BTreeSet<u64>,
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
/// docs/evidence/soak-starvation-evidence.log.
#[derive(Debug, Default)]
pub struct AdmissionQueue {
    pub(super) state: Mutex<QueueState>,
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
///
/// This is the per-database answer, as `/v1/status` serves it. The
/// percentiles here are this DATABASE's own, which is not the same question
/// as the service-wide percentiles in `MetricsSnapshot`: one database
/// contending hard while the rest of the service is idle is exactly the
/// situation a service-wide percentile hides.
#[derive(Debug, Default, Clone, Copy)]
pub struct DbQueue {
    pub waits: u64,
    pub wait_ms: u64,
    pub timeouts: u64,
    /// The longest single wait seen for this database, in microseconds.
    /// The average is what a total alone gives; the worst case is what
    /// an operator feels.
    pub max_wait_us: u64,
    /// The 95th percentile of this database's own recent admission waits,
    /// in microseconds. `None` when the database has never waited: a
    /// percentile of nothing is nothing, and reporting zero would claim
    /// the queue never contended.
    pub wait_p95_us: Option<u64>,
    /// The 95th percentile of this database's own recent statement
    /// executions, in microseconds. `None` when no statement has run
    /// against it.
    pub query_p95_us: Option<u64>,
}

impl Store {
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
}
