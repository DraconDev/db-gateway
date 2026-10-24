//! The durable record of which databases are forks of which.
//!
//! The copy itself is an ordinary database; this is the note that says
//! where it came from. It lives in the data directory next to the
//! registry, as JSON, and it is bounded: a fork record is kept for
//! every branch the operator made, and a branch that was deleted has
//! its record removed with it.
//!
//! WHY it is on disk and not only in memory: a branch is a fact about
//! the data, and the gateway gets replaced. A registry that lives in
//! process memory forgets every fork the moment the process restarts,
//! and the operator is left with databases whose origin nobody can
//! name -- the exact question a branch exists to answer.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The most branch records the registry keeps.
///
/// The registry is a file an operator can read and a program can load,
/// so it is bounded rather than endless. Past the cap the OLDEST record
/// is dropped, because the newest forks are the ones still being worked
/// on, and an unbounded file is a file nobody can reason about.
pub const MAX_BRANCHES: usize = 1024;

/// The registry file, inside the data directory.
pub const REGISTRY_FILE: &str = "branches.json";

/// The on-disk shape of the registry.
///
/// The version is written so a future format can be recognised rather
/// than misread: a file this gateway cannot understand is reported, not
/// parsed on a guess.
const REGISTRY_VERSION: u32 = 1;

/// The branches the operator has made.
#[derive(Debug, Default)]
pub struct BranchRegistry {
    branches: Mutex<HashMap<String, BranchRecord>>,
    /// Whether the on-disk copy has already been read into this registry.
    loaded: AtomicBool,
    /// Serializes the load, so two requests arriving together cannot
    /// both read the file and race each other's records.
    load_lock: Mutex<()>,
}

/// One branch, as it is written down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The file as it is written: a version and the records.
#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    branches: Vec<BranchRecord>,
}

impl BranchRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry as it is on disk.
    ///
    /// A missing file is an empty registry: nothing has been branched
    /// yet. A file that cannot be read is REPORTED, not hidden -- the
    /// operator is told the registry is broken rather than shown an
    /// empty list that looks like the truth. The error is logged here;
    /// [`BranchRegistry::load_into`] hands it back to a caller that can
    /// do something about it.
    pub fn load_from(root: &Path) -> Self {
        let registry = Self::new();
        if let Err(error) = registry.load_into(root) {
            tracing::error!(
                code = error.code,
                detail = %error,
                "the branch registry could not be read; this process starts with no branches"
            );
        }
        registry
    }

    /// Read the registry from disk into THIS registry.
    ///
    /// The store owns one registry for the life of the process, so the
    /// records have to arrive in place rather than in a replacement.
    /// The read happens once: a registry that has been read is never
    /// re-read, so a later request cannot resurrect a record the
    /// operator deleted after boot.
    pub fn load_into(&self, root: &Path) -> Result<()> {
        if self.loaded.load(Ordering::SeqCst) {
            return Ok(());
        }
        let _serialized = self.lock_load();
        if self.loaded.load(Ordering::SeqCst) {
            return Ok(());
        }
        let records = read_records(root)?;
        {
            let mut guard = self.lock();
            for record in records {
                guard.insert(record.name.clone(), record);
            }
        }
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Write the registry back to disk.
    ///
    /// Atomic: the bytes go to a temporary name, are flushed, and are
    /// then renamed over the old file. A crash in the middle leaves the
    /// previous registry intact rather than a half-written one, because
    /// a registry that half survived a crash is worse than one that did
    /// not survive it at all.
    pub fn persist(&self, root: &Path) -> Result<()> {
        let document = RegistryFile {
            version: REGISTRY_VERSION,
            branches: self.all(),
        };
        let bytes = serde_json::to_vec_pretty(&document).map_err(|_| Error::internal())?;
        write_atomic(root, &bytes)
    }

    /// Record a branch.
    ///
    /// The registry is bounded, so a record past the cap drops the
    /// oldest one rather than growing without limit.
    pub fn record(&self, record: BranchRecord) {
        let mut guard = self.lock();
        guard.insert(record.name.clone(), record);
        if guard.len() > MAX_BRANCHES {
            let oldest = guard
                .values()
                .min_by(|left, right| {
                    (left.forked_at, &left.name).cmp(&(right.forked_at, &right.name))
                })
                .map(|record| record.name.clone());
            if let Some(name) = oldest {
                guard.remove(&name);
                tracing::warn!(
                    branch = %name,
                    limit = MAX_BRANCHES,
                    "the branch registry is full; the oldest branch record was dropped"
                );
            }
        }
    }

    /// One branch, by name.
    pub fn get(&self, name: &str) -> Option<BranchRecord> {
        let guard = self.lock();
        guard.get(name).cloned()
    }

    /// Every branch, by name.
    pub fn all(&self) -> Vec<BranchRecord> {
        let guard = self.lock();
        let mut records: Vec<BranchRecord> = guard.values().cloned().collect();
        records.sort_by(|left, right| left.name.cmp(&right.name));
        records
    }

    /// Forget a branch.
    pub fn forget(&self, name: &str) {
        let mut guard = self.lock();
        guard.remove(name);
    }

    /// The records, with a poisoned lock recovered rather than fatal.
    ///
    /// A panic in another thread must not take the registry down with
    /// it: the records are still there, and the operator still needs to
    /// see them.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, BranchRecord>> {
        match self.branches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_load(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.load_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Read the records from `<root>/branches.json`.
///
/// A missing file is an empty registry. Anything else that goes wrong is
/// an error: a registry that cannot be read must not be mistaken for a
/// registry with no branches.
fn read_records(root: &Path) -> Result<Vec<BranchRecord>> {
    let path = root.join(REGISTRY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Error::with_message(
                500,
                "branch_registry_unreadable",
                format!(
                    "the branch registry at {} could not be read: {error}",
                    path.display()
                ),
            ));
        }
    };
    let document: RegistryFile = serde_json::from_slice(&bytes).map_err(|error| {
        Error::with_message(
            500,
            "branch_registry_corrupt",
            format!(
                "the branch registry at {} is not the JSON this gateway writes: {error}",
                path.display()
            ),
        )
    })?;
    if document.version != REGISTRY_VERSION {
        return Err(Error::with_message(
            500,
            "branch_registry_corrupt",
            format!(
                "the branch registry at {} is version {}; this gateway writes version {REGISTRY_VERSION}",
                path.display(),
                document.version
            ),
        ));
    }
    Ok(document.branches)
}

/// Write the registry file atomically, with permissions only its owner
/// can read.
fn write_atomic(root: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    if !root.exists() {
        std::fs::create_dir_all(root).map_err(|error| {
            Error::with_message(
                500,
                "internal_error",
                format!("the branch registry directory could not be created: {error}"),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    Error::with_message(
                        500,
                        "internal_error",
                        format!("the branch registry directory could not be made private: {error}"),
                    )
                },
            )?;
        }
    }
    let path = root.join(REGISTRY_FILE);
    let temp = root.join(format!("{REGISTRY_FILE}.tmp"));
    {
        let mut file = open_private(&temp).map_err(|error| {
            Error::with_message(
                500,
                "internal_error",
                format!("the branch registry could not be opened for writing: {error}"),
            )
        })?;
        file.write_all(bytes).map_err(|error| {
            Error::with_message(
                500,
                "internal_error",
                format!("the branch registry could not be written: {error}"),
            )
        })?;
        file.sync_all().map_err(|error| {
            Error::with_message(
                500,
                "internal_error",
                format!("the branch registry could not be flushed: {error}"),
            )
        })?;
    }
    std::fs::rename(&temp, &path).map_err(|error| {
        Error::with_message(
            500,
            "internal_error",
            format!("the branch registry could not be published: {error}"),
        )
    })?;
    // The rename has to be durable too, or a crash can lose the very
    // write that was just flushed.
    if let Ok(directory) = std::fs::File::open(root) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Open a file that only its owner can read.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}
