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
//!
//! The credential behind an action is recorded by its LABEL -- the name
//! the operator gave it when the key was issued -- and never by its
//! secret. A log that holds secrets hands them to everyone who reads
//! it, and the label is what an operator recognises anyway.
//!
//! A statement is recorded by its SHAPE: the SQL with every literal
//! replaced by a placeholder. The shape says which statement ran; the
//! values say what the data was, and that is not this log's business.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Principal;
use super::queries::normalise;

/// How many entries the audit log keeps.
pub const MAX_ENTRIES: usize = 2000;

/// One recorded action.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    /// When it happened, in seconds since the Unix epoch.
    ///
    /// The log keeps the order the actions happened in, so two actions
    /// in the same second are still the right way round. The second is
    /// the finest this stamp gets.
    pub at: u64,
    /// The credential that did it, by name.
    pub principal: String,
    /// The role that credential had.
    pub role: String,
    /// What it did: `query`, `create`, `delete`, `clone`, ...
    pub action: String,
    /// The database it acted on, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Anything else worth saying about it.
    ///
    /// For a statement this is the statement's shape, never its values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AuditEntry {
    /// An entry for an action by a credential.
    ///
    /// The credential contributes its name and its role and nothing
    /// else: its secret is not reachable from here, so it cannot be
    /// written down by accident.
    pub fn new(
        principal: &Principal,
        action: &str,
        database: Option<&str>,
        detail: Option<String>,
    ) -> Self {
        Self {
            at: now_secs(),
            principal: principal.name.clone(),
            role: principal.role.clone(),
            action: action.to_string(),
            database: database.map(str::to_string),
            detail,
        }
    }
}

/// The recorded actions, oldest first.
#[derive(Debug, Default)]
pub struct AuditLog {
    entries: Mutex<VecDeque<AuditEntry>>,
}

impl AuditLog {
    /// How many entries the log keeps.
    ///
    /// The same number as [`MAX_ENTRIES`], reachable from outside the
    /// store module: a caller that wants to prove the log is bounded
    /// needs the cap, not a guess at it.
    pub const MAX_ENTRIES: usize = MAX_ENTRIES;

    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one action.
    ///
    /// The oldest entry is dropped when the log is full: the newest
    /// actions are the ones an operator is asking about, and a log that
    /// grows without bound is a disk exhaustion like any other.
    ///
    /// A poisoned lock is recovered rather than propagated. A panic
    /// while the log was held must not cost the gateway its audit trail
    /// for the rest of its life, and an audit entry is not worth taking
    /// the service down for.
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

    /// Record an action by a credential, in one call.
    ///
    /// This is what the API calls: the credential, the action, the
    /// database it acted on, and whatever else is worth saying about
    /// it.
    pub fn record_action(
        &self,
        principal: &Principal,
        action: &str,
        database: Option<&str>,
        detail: Option<String>,
    ) {
        self.record(AuditEntry::new(principal, action, database, detail));
    }

    /// Record an action by a statement, keeping the statement's shape.
    ///
    /// The shape is the SQL with every literal replaced by a
    /// placeholder, so the log says which statement ran without
    /// becoming a second copy of the data that went through it.
    pub fn record_statement(
        &self,
        principal: &Principal,
        action: &str,
        database: Option<&str>,
        sql: &str,
    ) {
        self.record_action(principal, action, database, Some(normalise(sql)));
    }

    /// Every recorded action, oldest first.
    pub fn entries(&self) -> Vec<AuditEntry> {
        let guard = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.iter().cloned().collect()
    }

    /// The actions at or after a moment, oldest first.
    ///
    /// The moment is a Unix second, the same unit the entries carry.
    pub fn entries_since(&self, since: u64) -> Vec<AuditEntry> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.at >= since)
            .collect()
    }

    /// The actions of one credential, oldest first.
    ///
    /// The credential is named by its label, the way the operator named
    /// it when the key was issued.
    pub fn entries_for(&self, principal: &str) -> Vec<AuditEntry> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.principal == principal)
            .collect()
    }

    /// The actions that match the filters, oldest first.
    ///
    /// The limit keeps the NEWEST entries, because an operator asking
    /// for the last few actions means the last few. The order of what
    /// is left is unchanged: oldest first, like the log itself.
    pub fn entries_matching(
        &self,
        principal: Option<&str>,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Vec<AuditEntry> {
        let mut entries: Vec<AuditEntry> = self
            .entries()
            .into_iter()
            .filter(|entry| principal.is_none_or(|name| entry.principal == name))
            .filter(|entry| since.is_none_or(|since| entry.at >= since))
            .collect();
        if let Some(limit) = limit
            && entries.len() > limit
        {
            entries.drain(..entries.len() - limit);
        }
        entries
    }

    /// How many actions the log holds.
    pub fn len(&self) -> usize {
        let guard = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.len()
    }

    /// Whether the log holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Seconds since the Unix epoch, or zero if the clock is before it.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}
