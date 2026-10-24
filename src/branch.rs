//! Forks of a database: a copy that can diverge from what it came from.
//!
//! A branch is the thing a hosted provider will not give an operator:
//! a second copy of a database that starts at a known moment and then
//! goes its own way. Here the copy is made inside the gateway, from the
//! same files the live database uses, so the fork is the data and not
//! a description of it.
//!
//! What a branch is NOT is a backup. A branch is writable and it is
//! live: it diverges the moment it exists. The backup chain is what
//! keeps the past; a branch is what lets the future fork.

/// A branch's recorded facts.
///
/// The registry is the durable half of a branch: the copy itself is an
/// ordinary database in the store, and this says where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// The name of the branched database.
    pub name: String,
    /// The database it was forked from.
    pub parent: String,
    /// When the fork was taken, in seconds since the Unix epoch.
    pub forked_at: u64,
    /// Where the fork's contents came from.
    ///
    /// `live` is the database as it is now; `snapshot` is a backup set
    /// taken at a moment in the past.
    pub source: BranchSource,
}

/// Where a branch's contents came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchSource {
    /// Forked from the live database.
    Live,
    /// Forked from a backup set at a moment in the past.
    Snapshot {
        /// The backup set's timestamp, as the backup root names it.
        set: String,
    },
}
