//! The durable record of which databases are forks of which.
//!
//! The copy itself is an ordinary database; this is the note that says
//! where it came from. It lives in the data directory next to the
//! registry, as JSON, and it is bounded: a fork record is kept for
//! every branch the operator made, and a branch that was deleted has
//! its record removed with it.

use std::collections::HashMap;
use std::sync::Mutex;

/// The branches the operator has made.
#[derive(Debug, Default)]
pub struct BranchRegistry {
    branches: Mutex<HashMap<String, BranchRecord>>,
}

/// One branch, as it is written down.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BranchRecord {
    /// The name of the branched database.
    pub name: String,
    /// The database it was forked from.
    pub parent: String,
    /// When the fork was taken, in seconds since the Unix epoch.
    pub forked_at: u64,
    /// The backup set the fork came from, when it came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<String>,
}

impl BranchRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a branch.
    pub fn record(&self, record: BranchRecord) {
        let mut guard = match self.branches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.insert(record.name.clone(), record);
    }

    /// One branch, by name.
    pub fn get(&self, name: &str) -> Option<BranchRecord> {
        let guard = match self.branches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(name).cloned()
    }

    /// Every branch, by name.
    pub fn all(&self) -> Vec<BranchRecord> {
        let guard = match self.branches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut records: Vec<BranchRecord> = guard.values().cloned().collect();
        records.sort_by(|a, b| a.name.cmp(&b.name));
        records
    }

    /// Drop a branch that no longer exists.
    pub fn forget(&self, name: &str) {
        let mut guard = match self.branches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.remove(name);
    }
}
