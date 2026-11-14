//! Shadow mode: mirror an application's acknowledged writes to a dg
//! database in the background, and report -- never raise -- where the copy
//! disagrees. The Rust counterpart of `client/src/shadow.ts`; see
//! `docs/shadow.md`.
//!
//! The application's own database stays the source of truth. The caller
//! runs each write there first, exactly as before, and hands the statement
//! to [`Shadow::mirror`] only once that write has succeeded. `mirror`
//! never blocks and never awaits: it puts the statement on a bounded queue
//! that one background task drains in order, so dg sees writes in the
//! order the primary acknowledged them, and nothing dg does -- slow,
//! refusing, unreachable -- can reach the application's request.
//!
//! ```no_run
//! # async fn demo(db: dg_client::Database) {
//! use dg_client::shadow::{Shadow, ShadowOptions};
//! let shadow = Shadow::spawn(db, ShadowOptions::default());
//! // ...after the primary acknowledged a write that changed 1 row:
//! shadow.mirror(vec![dg_client::Statement::new("UPDATE t SET x = 1")], Some(vec![1]));
//! shadow.flush().await; // only in tests / before a comparison
//! # }
//! ```

use crate::{Database, Statement};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

/// What kind of disagreement a [`Divergence`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergenceKind {
    /// The primary accepted a write that dg refused or failed on.
    ShadowError,
    /// The same write changed a different number of rows.
    RowsAffectedMismatch,
    /// The statement cannot be mirrored as is (named parameters, a
    /// multi-statement script); it was not sent.
    Unsupported,
    /// The statement depends on time or randomness, so the copies may
    /// drift without either being wrong. It is still sent.
    Nondeterministic,
    /// The queue was full; the write was dropped and the copy is now behind.
    Dropped,
}

/// One disagreement between the primary and the dg copy.
#[derive(Clone, Debug)]
pub struct Divergence {
    pub kind: DivergenceKind,
    pub sql: String,
    pub detail: String,
}

/// How a [`Shadow`] behaves.
#[derive(Clone)]
pub struct ShadowOptions {
    /// How many mirrored operations may wait; beyond it new ones are
    /// dropped and reported as [`DivergenceKind::Dropped`].
    pub max_queue: usize,
    /// Called for every divergence, on the shadow's task or the caller's
    /// thread. Must not block for long. Default: a line on stderr.
    pub on_divergence: Arc<dyn Fn(Divergence) + Send + Sync>,
}

impl Default for ShadowOptions {
    fn default() -> Self {
        Self {
            max_queue: 10_000,
            on_divergence: Arc::new(|d| {
                eprintln!(
                    "dg shadow divergence: {:?}: {} -- {}",
                    d.kind, d.sql, d.detail
                )
            }),
        }
    }
}

/// Counters, for a status page or a log line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowStats {
    /// Statements dg applied.
    pub mirrored: u64,
    pub divergences: u64,
    pub dropped: u64,
}

enum Job {
    Write {
        statements: Vec<Statement>,
        rows_affected: Option<Vec<u64>>,
    },
    Flush(oneshot::Sender<()>),
}

#[derive(Default)]
struct Counters {
    mirrored: AtomicU64,
    divergences: AtomicU64,
    dropped: AtomicU64,
}

/// A handle to a running shadow. Cheap to clone; every clone feeds the same
/// queue, so the order of writes across clones is the order of `mirror`
/// calls.
#[derive(Clone)]
pub struct Shadow {
    queue: mpsc::Sender<Job>,
    counters: Arc<Counters>,
    report: Arc<dyn Fn(Divergence) + Send + Sync>,
}

impl std::fmt::Debug for Shadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shadow")
            .field("stats", &self.stats())
            .finish()
    }
}

/// Does `sql` compute a value from the clock or from randomness?
pub fn is_nondeterministic(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    [
        "random(",
        "randomblob(",
        "current_timestamp",
        "current_date",
        "current_time",
        "'now'",
        "\"now\"",
        "changes(",
        "last_insert_rowid(",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

impl Shadow {
    /// Start the background task on the current Tokio runtime.
    pub fn spawn(database: Database, options: ShadowOptions) -> Self {
        let (queue, mut jobs) = mpsc::channel::<Job>(options.max_queue.max(1));
        let counters = Arc::new(Counters::default());
        let report = options.on_divergence.clone();
        let shadow = Self {
            queue,
            counters: counters.clone(),
            report: report.clone(),
        };
        tokio::spawn(async move {
            while let Some(job) = jobs.recv().await {
                match job {
                    Job::Flush(done) => {
                        let _ = done.send(());
                    }
                    Job::Write {
                        statements,
                        rows_affected,
                    } => apply(&database, statements, rows_affected, &counters, &*report).await,
                }
            }
        });
        shadow
    }

    /// Queue a write the primary has acknowledged. `rows_affected`, when
    /// given, is what the primary reported per statement, and is compared
    /// with dg's. Several statements are applied as one atomic dg batch.
    /// Never blocks.
    pub fn mirror(&self, statements: Vec<Statement>, rows_affected: Option<Vec<u64>>) {
        if statements.is_empty() {
            return;
        }
        for statement in &statements {
            if is_nondeterministic(&statement.sql) {
                self.diverge(
                    DivergenceKind::Nondeterministic,
                    &statement.sql,
                    "depends on time or randomness; dg computes its own value",
                );
            }
        }
        let label = statements
            .iter()
            .map(|s| s.sql.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if self
            .queue
            .try_send(Job::Write {
                statements,
                rows_affected,
            })
            .is_err()
        {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.diverge(
                DivergenceKind::Dropped,
                &label,
                "shadow queue full or closed; the write was dropped and the copy is behind",
            );
        }
    }

    /// Report a statement that cannot be mirrored (for the caller's own
    /// adapters: named parameters, scripts).
    pub fn unsupported(&self, sql: &str, why: &str) {
        self.diverge(DivergenceKind::Unsupported, sql, why);
    }

    /// Resolves once every write queued before this call has been applied.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.queue.send(Job::Flush(done)).await.is_ok() {
            let _ = wait.await;
        }
    }

    pub fn stats(&self) -> ShadowStats {
        ShadowStats {
            mirrored: self.counters.mirrored.load(Ordering::Relaxed),
            divergences: self.counters.divergences.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
        }
    }

    fn diverge(&self, kind: DivergenceKind, sql: &str, detail: &str) {
        self.counters.divergences.fetch_add(1, Ordering::Relaxed);
        (self.report)(Divergence {
            kind,
            sql: sql.to_string(),
            detail: detail.to_string(),
        });
    }
}

async fn apply(
    database: &Database,
    statements: Vec<Statement>,
    rows_affected: Option<Vec<u64>>,
    counters: &Counters,
    report: &(dyn Fn(Divergence) + Send + Sync),
) {
    let diverge = |kind, sql: &str, detail: String| {
        counters.divergences.fetch_add(1, Ordering::Relaxed);
        report(Divergence {
            kind,
            sql: sql.to_string(),
            detail,
        });
    };
    let label = statements
        .iter()
        .map(|s| s.sql.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let count = statements.len() as u64;
    let results = if let [only] = statements.as_slice() {
        database
            .execute(&only.sql, only.args.clone())
            .await
            .map(|r| vec![r])
    } else {
        database.batch(statements.clone()).await
    };
    let results = match results {
        Ok(results) => results,
        Err(error) => {
            diverge(
                DivergenceKind::ShadowError,
                &label,
                format!("{}: {}", error.code, error.message),
            );
            return;
        }
    };
    counters.mirrored.fetch_add(count, Ordering::Relaxed);
    if let Some(expected) = rows_affected {
        for ((statement, result), want) in statements.iter().zip(&results).zip(expected) {
            if result.rows_affected != want {
                diverge(
                    DivergenceKind::RowsAffectedMismatch,
                    &statement.sql,
                    format!("primary {want}, dg {}", result.rows_affected),
                );
            }
        }
    }
}
