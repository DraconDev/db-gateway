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
//! It is also DURABLE, which the query log deliberately is not. The
//! question this log answers is asked after something unexpected, and
//! a restart is very often the unexpected thing: a gateway that comes
//! back up with an empty trail cannot say which credential was doing
//! what when it went down. The trail is therefore appended to a file
//! in the data directory as it happens, and the entries that file
//! holds are read back when the gateway starts. The query log answers
//! a present-tense question -- which statement is heavy right now --
//! so it stays in memory, where a restart costs nothing.
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
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Principal;
use super::queries::normalise;

/// How many entries the audit log keeps.
pub const MAX_ENTRIES: usize = 2000;

/// How large the trail file may grow before it is rewritten.
///
/// The file holds the log, so it has to be bounded like the log: a
/// trail that grows without limit is a disk exhaustion like any other.
/// When the file passes this size it is rewritten from the entries the
/// log still holds, which is a bounded number, so the file cannot grow
/// without limit. The cap is generous because the entries are small
/// and an operator does not want a trail that turns over.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// One recorded action.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Anything else worth saying about it.
    ///
    /// For a statement this is the statement's shape, never its values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// How long the statement took, in microseconds, when the entry is
    /// for a statement that ran.
    ///
    /// Anything that is not a statement carries no time: a `create` did
    /// not run, so there is nothing to time. Absent (`None`) also reads
    /// the trail lines written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_us: Option<u64>,
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
            elapsed_us: None,
        }
    }
}

/// The recorded actions, oldest first.
#[derive(Debug, Default)]
pub struct AuditLog {
    entries: Mutex<VecDeque<AuditEntry>>,
    /// The file the trail is appended to, when the log has one.
    ///
    /// The lock is separate from the entries' because the file is
    /// written while the entries are held: the file and the ring must
    /// never disagree about what was recorded. The order is always the
    /// entries first and the file second, so the two cannot deadlock.
    trail: Mutex<Option<Trail>>,
}

/// The file the trail lives in.
#[derive(Debug)]
struct Trail {
    /// Where the file is.
    path: PathBuf,
    /// The file, opened for appending.
    file: File,
    /// How large the file is, so the cap can be checked without asking
    /// the filesystem on every entry.
    bytes: u64,
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

    /// A log whose trail lives in `path`.
    ///
    /// The file is read back first, so the log starts with the entries
    /// the previous run of the gateway recorded: a gateway that comes
    /// back up after a restart can still say which credential was
    /// doing what when it went down. That is the whole reason the
    /// trail is on disk.
    ///
    /// A file that cannot be opened is a warning, not a failure: the
    /// gateway still serves, and still keeps the trail in memory for
    /// as long as it runs. Losing the trail across a restart is a
    /// smaller problem than refusing to start.
    pub fn open(path: &Path) -> Self {
        let entries = Self::read_trail(path);
        let trail = match Self::open_trail(path) {
            Ok(trail) => Some(trail),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "the audit trail file could not be opened; the trail will not survive a restart"
                );
                None
            }
        };
        Self {
            entries: Mutex::new(entries),
            trail: Mutex::new(trail),
        }
    }

    /// The entries a previous run of the gateway left behind.
    ///
    /// A line that does not parse is skipped rather than fatal: a
    /// process killed mid-write leaves a torn last line, and the
    /// entries before it are still worth having.
    fn read_trail(path: &Path) -> VecDeque<AuditEntry> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return VecDeque::new();
        };
        let mut entries = VecDeque::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<AuditEntry>(line) else {
                continue;
            };
            entries.push_back(entry);
        }
        while entries.len() > MAX_ENTRIES {
            entries.pop_front();
        }
        entries
    }

    /// Open the trail file for appending.
    fn open_trail(path: &Path) -> std::io::Result<Trail> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            // The mode applies at creation, so the file is never even
            // briefly readable by anyone else. The trail names
            // credentials and records what they did: it is the
            // operator's business, and the directory around it is
            // 0700, but the file itself is 0600 as well.
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Trail {
            path: path.to_path_buf(),
            file,
            bytes,
        })
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
        // The line is rendered before the lock is taken: serialising
        // does not need the log, and the lock should be held for as
        // little as possible.
        let line = serde_json::to_string(&entry).ok();
        let mut guard = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push_back(entry);
        while guard.len() > MAX_ENTRIES {
            guard.pop_front();
        }
        if let Some(line) = line {
            self.append(&line, &guard);
        }
    }

    /// Append one entry to the trail file.
    ///
    /// The write happens while the entries are held, so the file and
    /// the ring never disagree about what was recorded. A write that
    /// fails is a warning, not an error: the trail is worth keeping,
    /// but it is not worth taking the gateway down for.
    fn append(&self, line: &str, entries: &VecDeque<AuditEntry>) {
        let mut guard = match self.trail.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(trail) = guard.as_mut() else {
            return;
        };
        let size = line.len() as u64 + 1;
        if trail.bytes + size > MAX_FILE_BYTES {
            // The rewrite writes the WHOLE ring, and the ring already
            // holds the entry that is being recorded -- so the rewrite
            // has already written this line. Appending it again would
            // leave the action that triggered the rollover in the file
            // twice, and the operator would read one action as two.
            if !trail.rewrite(entries) {
                return;
            }
            return;
        }
        let written = trail
            .file
            .write_all(line.as_bytes())
            .and_then(|()| trail.file.write_all(b"\n"))
            .and_then(|()| trail.file.flush());
        if let Err(error) = written {
            tracing::warn!(%error, "the audit trail could not be appended to");
            return;
        }
        trail.bytes += size;
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
    /// becoming a second copy of the data that went through it. The
    /// elapsed time is the measured cost of that run, or `None` when
    /// the caller has no measurement to offer.
    pub fn record_statement(
        &self,
        principal: &Principal,
        action: &str,
        database: Option<&str>,
        sql: &str,
        elapsed_us: Option<u64>,
    ) {
        let mut entry = AuditEntry::new(principal, action, database, Some(normalise(sql)));
        entry.elapsed_us = elapsed_us;
        self.record(entry);
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

impl Trail {
    /// Rewrite the file from the entries the log still holds.
    ///
    /// The file is written beside the original and renamed over it, so
    /// a crash mid-rewrite leaves the old file whole rather than a
    /// half-written one. The entries the log still holds are a bounded
    /// number, so the file cannot grow without limit.
    fn rewrite(&mut self, entries: &VecDeque<AuditEntry>) -> bool {
        let temp = self.path.with_extension("jsonl.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let Ok(file) = options.open(&temp) else {
            return false;
        };
        let mut writer = BufWriter::new(file);
        let mut bytes = 0u64;
        for entry in entries {
            let Ok(line) = serde_json::to_string(entry) else {
                continue;
            };
            if writer
                .write_all(line.as_bytes())
                .and_then(|()| writer.write_all(b"\n"))
                .is_err()
            {
                return false;
            }
            bytes += line.len() as u64 + 1;
        }
        if writer.flush().is_err() {
            return false;
        }
        drop(writer);
        if std::fs::rename(&temp, &self.path).is_err() {
            return false;
        }
        let Ok(file) = OpenOptions::new().append(true).open(&self.path) else {
            return false;
        };
        self.file = file;
        self.bytes = bytes;
        true
    }
}

/// Seconds since the Unix epoch, or zero if the clock is before it.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}
