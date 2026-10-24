//! Who ran what, and when.
//!
//! The audit log is the answer to the question an operator asks after
//! something unexpected: which credential did this, and when? It is a
//! record of ACTIONS -- a credential asked for a database, ran a
//! statement, created a key -- and never of the data that moved.
//!
//! It is bounded in the same way the query log is: the log keeps a
//! fixed number of entries and drops the oldest, because a log that
//! fills the disk is the same failure as any other disk exhaustion.

use std::collections::VecDeque;
use std::sync::Mutex;

/// How many entries the audit log keeps.
pub const MAX_ENTRIES: usize = 2000;

/// One recorded action.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    /// When it happened, in seconds since the Unix epoch.
    pub at: u64,
    /// The credential that did it.
    pub principal: String,
    /// The role that credential had.
    pub role: String,
    /// What it did: `query`, `create`, `delete`, `clone`, ...
    pub action: String,
    /// The database it acted on, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Anything else worth saying about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The recorded actions, oldest first.
#[derive(Debug, Default)]
pub struct AuditLog {
    entries: Mutex<VecDeque<AuditEntry>>,
}

impl AuditLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one action.
    pub fn record(&self, entry: AuditEntry) {
        let mut guard = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push_back(entry);
        while guard.len() > MAX_ENTRIES {
            guard.pop_front();
        }
    }

    /// Every recorded action, oldest first.
    pub fn entries(&self) -> Vec<AuditEntry> {
        let guard = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.iter().cloned().collect()
    }
}
