//! Backup freshness, computed once for every consumer.
//!
//! The exposure half (`GET /v1/backups`, `dg backup status`) shipped first
//! and the delivery half (observability contract §3 alert 4) follows it. If
//! the two halves each carried their own copy of the freshness rule, the
//! digest an operator receives could disagree with the endpoint they check
//! against, and the disagreement would be invisible until it mattered. So the
//! rule lives here and both halves call it.
//!
//! What "fresh" means is contract §2 and is not a policy this module gets to
//! choose: the age of the newest verified copy, never the start time of a
//! scheduled job. A database with no containing set, no durable write
//! marker, or no coverage boundary is `unknown` -- never silently `fresh`,
//! because "unknown" is the state that keeps a scheduled job honest.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Staleness reference (observability contract §2): committed changes
/// unrepresented by a verified copy for longer than this deserve an operator
/// look. An objective, not an accepted RPO.
pub const STALE_AFTER_SECS: u64 = 300;

/// Per-database freshness. `state` is exactly one of `fresh`, `stale`,
/// `unknown`, matching `/v1/backups`; the two are the same value computed
/// once, so an operator cannot be told two different things.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseHealth {
    pub name: String,
    pub state: &'static str,
    /// The set this state was derived from, or `None` for `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_set: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
}

impl DatabaseHealth {
    pub fn is_attention(&self) -> bool {
        self.state != "fresh"
    }
}

/// One record from `$BACKUP_ROOT/attempts.jsonl`, appended by the producer
/// (`scripts/micro2-backup.sh`) on every run. This is the only durable
/// evidence of an attempt that FAILED: a failed run publishes no set, so
/// "no new set" cannot distinguish a failure from a job that never ran, and
/// "immediate notice on failure" needs the attempt record to say so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// The run's version timestamp; also its identity, so delivery can
    /// recognise an attempt it has already reported.
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub completed_at: u64,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub set: Option<String>,
    #[serde(default)]
    pub databases: u64,
    /// The step reached when the run ended; the closest thing to a failure
    /// reason the producer records.
    #[serde(default)]
    pub step: String,
}

impl AttemptRecord {
    /// A failed run publishes nothing, so anything but a completed success
    /// is a failure worth noticing. An `error` outcome with a published set
    /// name is still a failure: the record is the producer's claim, and a
    /// claim that contradicts itself is not evidence of health.
    pub fn failed(&self) -> bool {
        self.outcome != "ok"
    }

    /// Identity for de-duplication. Empty timestamps (a producer that died
    /// before recording one) still de-duplicate, because re-notifying the
    /// same empty identity on every poll is exactly the spam this avoids.
    pub fn identity(&self) -> &str {
        &self.ts
    }
}

/// What the newest published set carries.
///
/// The per-database freshness answers "is there a set that contains this
/// database". This answers a different question: does the newest set
/// carry the whole deployment, registry included. A set without the
/// registry holds the data but not the credentials and scopes that make
/// it usable, so a restore from it gives back files that no key can open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// The live databases.
    pub live: usize,
    /// How many of them the newest set carries.
    pub in_newest_set: usize,
    /// The ones it does not, by name.
    pub missing: Vec<String>,
    /// Whether the newest set carries the registry.
    pub registry: bool,
}

/// Where the off-host copy is, as the host reports it.
///
/// The gateway does not know where the bucket is: the sidecar's config is
/// the host's business, and the host already publishes what it rendered
/// (`scripts/litestream-sync.sh` writes this file into the backup root).
/// The gateway reads it the same way it reads the attempt log, so an
/// operator sees one answer wherever it is asked for.
///
/// A file that is absent means "this host does not report its replica",
/// which is not the same claim as "there is no replica".
///
/// The fields are a whitelist on purpose. The publisher writes bucket,
/// endpoint and region -- never the access key or the secret -- and a
/// file carrying anything else is read without those extra values ever
/// reaching a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaFacts {
    /// When the host published this.
    pub published_at: u64,
    /// Where the copy is: a bucket name, a URL template, a file path.
    pub target: String,
    /// The bucket, when the replica is an object store.
    #[serde(default)]
    pub bucket: Option<String>,
    /// The endpoint the bucket is reached through.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The region the bucket lives in.
    #[serde(default)]
    pub region: Option<String>,
    /// How long the bucket keeps its history.
    #[serde(default)]
    pub retention: Option<String>,
    /// How often a snapshot is taken.
    #[serde(default)]
    pub snapshot_interval: Option<String>,
    /// The databases the replica covers.
    #[serde(default)]
    pub databases: Vec<String>,
    /// Whether the registry is replicated as well.
    #[serde(default)]
    pub registry: bool,
    /// Whether the replication sidecar is running: `active`, `inactive`,
    /// `failed`, or `unknown` when the host cannot tell.
    #[serde(default)]
    pub sidecar: Option<String>,
}

/// The result of the last off-host restore drill.
///
/// A backup that was never restored is an assumption; a drill that
/// restored the copies and compared them against the live gateway is
/// evidence. The drill runs on the host (it needs the bucket's
/// credentials and the sidecar), and it publishes its result here for the
/// same reason the replica publishes its settings: the operator asks the
/// gateway, and the gateway says what the host last proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrillFacts {
    /// When the drill ran.
    pub drilled_at: u64,
    /// `ok` when every database restored and verified, `failed` otherwise.
    pub outcome: String,
    /// How many databases the drill restored and verified.
    #[serde(default)]
    pub databases: u64,
    /// What the drill reported, in its own words.
    #[serde(default)]
    pub detail: Option<String>,
}

impl DrillFacts {
    /// Whether the drill proved the copies restore.
    pub fn ok(&self) -> bool {
        self.outcome == "ok"
    }
}

/// Read a JSON file the host published, if it is there and it parses.
///
/// Tolerant by construction, like the attempt log: the file is written by
/// another process, so a half-written one is normal rather than
/// corruption, and a view must not fail to render because of it. A file
/// that does not parse is treated as no file at all -- the answer is then
/// "not reported", which is honest.
fn read_published<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Where the off-host copy is, as the host last published it.
pub fn read_replica(root: &std::path::Path) -> Option<ReplicaFacts> {
    read_published(&root.join("replica.json"))
}

/// The result of the last off-host restore drill.
pub fn read_drill(root: &std::path::Path) -> Option<DrillFacts> {
    read_published(&root.join("restore-drill.json"))
}

/// The window of moments the off-host replica can be restored to.
///
/// "There is a backup" is a claim. "You can go back to any moment
/// between these two" is the claim that matters at two in the morning,
/// and it is a different one: the replica keeps a rolling window of
/// write-ahead log, and the width of that window is a property of the
/// bucket and the sidecar -- both of which belong to the host. So the
/// host publishes the window, the way it publishes where the copy is
/// and what the drill proved, and the gateway reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePoints {
    /// When the host published this.
    pub published_at: u64,
    /// The earliest moment that can be restored to.
    #[serde(default)]
    pub window_start: Option<String>,
    /// The newest moment the published files cover.
    #[serde(default)]
    pub window_end: Option<String>,
    /// The sidecar's state: `active`, `inactive`, `failed`, or
    /// `unknown` when the host cannot tell.
    #[serde(default)]
    pub sidecar: Option<String>,
    /// What each replicated database's own window looks like.
    #[serde(default)]
    pub databases: BTreeMap<String, RestoreWindow>,
}

/// One database's restore window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreWindow {
    /// The oldest file the replica holds for this database.
    #[serde(default)]
    pub oldest: Option<String>,
    /// The newest file the replica holds for this database.
    #[serde(default)]
    pub newest: Option<String>,
    /// How many files make up the window.
    #[serde(default)]
    pub files: u64,
    /// How many bytes those files hold.
    #[serde(default)]
    pub bytes: u64,
}

/// The result of the last point-in-time restore taken from the replica.
///
/// A window an operator is told about and never exercises is a promise.
/// A restore that was taken, verified and published is evidence, in the
/// same way the drill is: the host does it (it holds the bucket's
/// credentials), and the gateway reports what the host last did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreResult {
    /// When the restore ran.
    pub restored_at: u64,
    /// Which database was restored.
    pub database: String,
    /// The moment it was restored to.
    pub timestamp: String,
    /// `ok` when the copy restored and verified, `failed` otherwise.
    pub outcome: String,
    /// What the restore reported, in its own words.
    #[serde(default)]
    pub detail: Option<String>,
    /// How many user tables the restored copy holds.
    #[serde(default)]
    pub tables: u64,
    /// How many rows it holds.
    #[serde(default)]
    pub rows: u64,
    /// How large the restored file is.
    #[serde(default)]
    pub size_bytes: u64,
    /// What SQLite's own integrity check said about it.
    #[serde(default)]
    pub integrity: Option<String>,
    /// Where the restored copy is on the host.
    #[serde(default)]
    pub path: Option<String>,
}

impl RestoreResult {
    /// Whether the restore produced a verified copy.
    pub fn ok(&self) -> bool {
        self.outcome == "ok"
    }
}

/// The window of moments the replica can be restored to.
pub fn read_restore_points(root: &std::path::Path) -> Option<RestorePoints> {
    read_published(&root.join("restore-points.json"))
}

/// The result of the last point-in-time restore.
pub fn read_restore_result(root: &std::path::Path) -> Option<RestoreResult> {
    read_published(&root.join("restore-results.json"))
}

/// The whole snapshot: what the digest reports and what the endpoint serves.
#[derive(Debug, Clone, Serialize)]
pub struct BackupHealth {
    pub backup_root: Option<PathBuf>,
    pub stale_after_secs: u64,
    pub generated_at: u64,
    /// Per-database freshness, for the LIVE databases.
    ///
    /// A database that has been deleted is not in here: it is not stale,
    /// it is gone. The count of those the published sets still carry is
    /// in [`BackupHealth::deleted`].
    pub databases: Vec<DatabaseHealth>,
    /// Databases that were deleted and are still carried by the published
    /// sets. They are counted rather than listed: a deleted database does
    /// not need a backup, and listing it beside the live ones would say
    /// "stale" about something that was deleted on purpose.
    pub deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<AttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<AttemptRecord>,
    /// What the newest published set carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// Where the off-host copy is, as the host reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<ReplicaFacts>,
    /// The last off-host restore drill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drill: Option<DrillFacts>,
    /// The window of moments the off-host replica can be restored to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_points: Option<RestorePoints>,
    /// The last point-in-time restore taken from the replica.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_restore: Option<RestoreResult>,
}

impl BackupHealth {
    /// Databases an operator must act on: everything that is not `fresh`.
    pub fn attention(&self) -> Vec<&DatabaseHealth> {
        self.databases
            .iter()
            .filter(|db| db.is_attention())
            .collect()
    }

    /// Count per state, for a digest header that stays readable when the
    /// database list is long.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut fresh = 0;
        let mut stale = 0;
        let mut unknown = 0;
        for db in &self.databases {
            match db.state {
                "fresh" => fresh += 1,
                "stale" => stale += 1,
                _ => unknown += 1,
            }
        }
        (fresh, stale, unknown)
    }
}

/// The freshness rule, in one place, for both halves of the signal.
///
/// Ported from the `/v1/backups` handler unchanged; see that handler's own
/// comment for why the containing set is chosen by durable coverage boundary
/// rather than by publication time.
pub fn snapshot(store: &Store) -> Result<BackupHealth, crate::error::Error> {
    let now = SystemTime::now();
    let sets = store.backup_sets();
    // The live names are kept apart from the wider list below, which also
    // carries databases that only exist in a set. Coverage is a claim
    // about the live deployment, so it is counted from the live names.
    let live_names: Vec<String> = store.list_all()?;
    // A database that has been DELETED is not a database whose copies are
    // stale: it is gone, and the older sets still carry it until they are
    // pruned. Reporting those as `stale` made the daily digest exit
    // `backup_unhealthy` over databases that had been deleted on purpose,
    // and it pushed the live per-database rows off the operator's screen
    // behind rows for things that no longer exist. The freshness claim is
    // about the live deployment, so it is computed from the live names;
    // how many deleted ones the sets still carry is counted and published,
    // so nothing is hidden by being dropped.
    let mut deleted: Vec<String> = Vec::new();
    if let Some(ref sets) = sets {
        for set in sets {
            for db in &set.databases {
                if !live_names.contains(db) && !deleted.contains(db) {
                    deleted.push(db.clone());
                }
            }
        }
    }
    let mut names: Vec<String> = live_names.clone();
    if let Some(ref sets) = sets {
        for set in sets {
            for db in &set.databases {
                if !names.contains(db) {
                    names.push(db.clone());
                }
            }
        }
    }
    let mut databases = Vec::with_capacity(names.len());
    for name in &names {
        let newest = sets.as_ref().and_then(|sets| {
            let mut candidates = sets.iter().filter(|set| set.databases.contains(name));
            let newest = candidates.next()?;
            let mut selected = newest;
            for candidate in candidates {
                let replace = match (selected.coverage_start, candidate.coverage_start) {
                    (Some(current), Some(next)) => next > current,
                    (Some(_), None) => false,
                    (None, Some(_)) => true,
                    (None, None) => candidate.mtime > selected.mtime,
                };
                if replace {
                    selected = candidate;
                }
            }
            // `sets` arrives newest-first and `selected` starts as the
            // newest containing set, only ever replaced toward older ones,
            // so no containing set can have a greater mtime: no fallback arm
            // is needed here. (A previous revision carried a defensive
            // `mtime > selected.mtime` guard that could never fire.)
            Some(selected)
        });
        let (state, newest_set, age_secs) = match newest {
            None => ("unknown", None, None),
            Some(set) => {
                let age = now
                    .duration_since(set.mtime)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // The boundary is captured before any export. A write
                // at/after it cannot be proven included, even when the set
                // directory is published later. Missing durable boundary or
                // write marker is unknown, never falsely fresh after restart
                // or an older script.
                let state = match (store.last_write(name), set.coverage_start) {
                    (Some(wrote), Some(started)) if wrote >= started => "stale",
                    (Some(_), Some(_)) => "fresh",
                    _ => "unknown",
                };
                (state, Some(set.timestamp.clone()), Some(age))
            }
        };
        databases.push(DatabaseHealth {
            name: name.clone(),
            state,
            newest_set,
            age_secs,
        });
    }
    databases.sort_by(|left, right| left.name.cmp(&right.name));
    let root = store.backup_root();
    let (last_attempt, last_failure) = match root.as_deref() {
        Some(root) => {
            let attempts = read_attempts(root);
            let last = attempts.last().cloned();
            let failure = attempts.iter().rev().find(|a| a.failed()).cloned();
            (last, failure)
        }
        None => (None, None),
    };
    // What the newest set carries, and what the host reports about the
    // off-host copy and the last drill. Both are absent when the host
    // does not publish them, and absent is reported as absent.
    let coverage = sets.as_ref().and_then(|sets| {
        let newest = sets.first()?;
        let missing: Vec<String> = live_names
            .iter()
            .filter(|name| !newest.databases.contains(name))
            .cloned()
            .collect();
        Some(Coverage {
            live: live_names.len(),
            in_newest_set: live_names.len() - missing.len(),
            missing,
            registry: newest.has_registry,
        })
    });
    let (replica, drill) = match root.as_deref() {
        Some(root) => (read_replica(root), read_drill(root)),
        None => (None, None),
    };
    let (restore_points, last_restore) = match root.as_deref() {
        Some(root) => (read_restore_points(root), read_restore_result(root)),
        None => (None, None),
    };
    Ok(BackupHealth {
        backup_root: root,
        stale_after_secs: STALE_AFTER_SECS,
        generated_at: now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        databases,
        deleted: deleted.len(),
        last_attempt,
        last_failure,
        coverage,
        replica,
        drill,
        restore_points,
        last_restore,
    })
}

/// How much of the attempt log a read considers. The log is append-only and
/// never pruned, so at a five-minute cadence it grows by one line every five
/// minutes forever; re-reading all of it on every failure poll would spend
/// the notifier's whole budget on I/O it does not need. Every consumer wants
/// the newest records -- `last_attempt` and `last_failure` are both in the
/// tail -- so the read is bounded at the end.
const ATTEMPT_TAIL_BYTES: u64 = 64 * 1024;

/// Read the producer's attempt log, newest end. Tolerant by construction:
/// the file is appended to by a separate process while a backup runs, so a
/// torn final line is normal, not corruption, and a digest must not fail to
/// render because the newest line is half-written. Unparseable lines are
/// skipped, and a missing log is an empty log -- "no attempts recorded yet"
/// is a reportable state, not an error. Records older than the tail window
/// are not returned; the file itself remains the complete history.
pub fn read_attempts(root: &std::path::Path) -> Vec<AttemptRecord> {
    let path = root.join("attempts.jsonl");
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(ATTEMPT_TAIL_BYTES);
    if start > 0 {
        // The first line in the window is almost certainly a partial record,
        // so it is dropped rather than parsed.
        if std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
    }
    let mut text = String::new();
    if std::io::Read::read_to_string(&mut file, &mut text).is_err() {
        return Vec::new();
    }
    let body = if start == 0 {
        text.as_str()
    } else {
        match text.split_once('\n') {
            Some((_, rest)) => rest,
            // A window with no newline at all is one enormous line; there is
            // nothing complete to read in it.
            None => "",
        }
    };
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<AttemptRecord>(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_with_backup_root(dir: &std::path::Path) -> Store {
        let root = dir.join("dg-backups");
        Store::open(&dir.join("data"))
            .unwrap()
            .with_backup_root(Some(root))
    }

    /// `coverage_start` of `None` writes a manifest with no usable
    /// `started_at`, which is what a set published by an older script (or by
    /// hand) looks like to the reader.
    fn write_set(
        root: &std::path::Path,
        stamp: &str,
        databases: &[&str],
        coverage_start: Option<u64>,
    ) {
        let set = root.join(stamp);
        std::fs::create_dir_all(&set).unwrap();
        let (started, completed) = match coverage_start {
            Some(seconds) => (json!(seconds), json!(seconds + 1)),
            None => (serde_json::Value::Null, serde_json::Value::Null),
        };
        std::fs::write(
            set.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "started_at": started,
                "completed_at": completed,
                "registry_captured_at": completed,
                "databases": databases
                    .iter()
                    .map(|name| serde_json::json!({
                        "name": name, "file": format!("{name}.sqlite"),
                        "size": 1, "sha256": "0".repeat(64),
                    }))
                    .collect::<Vec<_>>(),
                "registry": {"file": "metadata.sqlite.bak", "size": 1, "sha256": "0".repeat(64)},
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn unknown_when_no_root_and_unknown_when_no_set() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        let health = snapshot(&store).unwrap();
        assert_eq!(health.databases.len(), 1);
        assert_eq!(health.databases[0].name, "alpha");
        assert_eq!(health.databases[0].state, "unknown");
        assert_eq!(health.databases[0].newest_set, None);
        assert_eq!(health.databases[0].age_secs, None);
        assert_eq!(health.attention().len(), 1);
    }

    #[test]
    fn fresh_when_the_write_predates_the_coverage_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        let root = dir.path().join("dg-backups");
        let boundary = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        write_set(&root, "20240101T000000Z", &["alpha"], Some(boundary));
        let health = snapshot(&store).unwrap();
        assert_eq!(health.databases[0].state, "fresh");
        assert_eq!(
            health.databases[0].newest_set.as_deref(),
            Some("20240101T000000Z")
        );
        assert!(health.attention().is_empty());
    }

    /// A deleted database is not a stale database.
    ///
    /// The health used to judge every database that appeared in a published
    /// set, including the ones that had been deleted. Those were counted
    /// as `stale`, which made the daily digest exit `backup_unhealthy` over
    /// databases that had been deleted on purpose, and it pushed the live
    /// rows off the operator's screen behind rows for things that no longer
    /// exist. A deleted database is gone, not stale.
    #[test]
    fn a_deleted_database_is_not_reported_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        store.create_and_note("gone").unwrap();
        let root = dir.path().join("dg-backups");
        let boundary = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        // The set carries both: one that is live and one that is not.
        write_set(&root, "20240101T000000Z", &["alpha", "gone"], Some(boundary));
        // And the second one was deleted on purpose.
        store.delete("gone", 3600).unwrap();

        let health = snapshot(&store).unwrap();
        // The live database is judged as it always was.
        assert_eq!(health.databases.len(), 1);
        assert_eq!(health.databases[0].name, "alpha");
        assert_eq!(health.databases[0].state, "fresh");
        // The deleted one asks for nothing: it does not need a backup.
        assert!(
            health.attention().is_empty(),
            "a deleted database asked for attention: {:?}",
            health.attention()
        );
        // It is counted, so it is not hidden by being dropped.
        assert_eq!(health.deleted, 1);
    }

    #[test]
    fn stale_when_a_write_lands_after_the_coverage_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        let root = dir.path().join("dg-backups");
        // A boundary in the past, so the durable write marker is newer than
        // the coverage it would need.
        let boundary = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60;
        write_set(&root, "20240101T000000Z", &["alpha"], Some(boundary));
        let health = snapshot(&store).unwrap();
        assert_eq!(health.databases[0].state, "stale");
        assert_eq!(health.attention().len(), 1);
    }

    #[test]
    fn a_set_with_no_coverage_boundary_is_unknown_never_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        let root = dir.path().join("dg-backups");
        // started_at absent: the manifest parses, so the set is visible, but
        // there is no durable boundary to compare a write against.
        write_set(&root, "20240101T000000Z", &["alpha"], None);
        let health = snapshot(&store).unwrap();
        assert_eq!(
            health.databases[0].state, "unknown",
            "a set that cannot prove coverage must never read as fresh"
        );
    }

    #[test]
    fn a_newer_unbounded_set_does_not_replace_a_proven_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        let root = dir.path().join("dg-backups");
        // The older set proves coverage, past the last write.
        let boundary = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        write_set(&root, "20240101T000000Z", &["alpha"], Some(boundary));
        // The newer set is published later but proves nothing, so it cannot
        // become the newest boundary and it cannot downgrade a proven one to
        // a false fresh. The proven boundary still governs.
        write_set(&root, "20240102T000000Z", &["alpha"], None);
        let health = snapshot(&store).unwrap();
        let alpha = &health.databases[0];
        assert_eq!(
            alpha.newest_set.as_deref(),
            Some("20240101T000000Z"),
            "the set that carries a durable boundary is the one that counts"
        );
        assert_eq!(alpha.state, "fresh");
    }

    #[test]
    fn a_torn_final_attempt_line_does_not_hide_the_last_good_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("attempts.jsonl"),
            concat!(
                r#"{"ts":"20240101T000000Z","started_at":1,"completed_at":2,"outcome":"ok","set":"20240101T000000Z","databases":2,"step":"publish"}"#,
                "\n",
                r#"{"ts":"20240102T000000Z","started_at":3,"completed_at":4,"outcome":"error","set":null,"databases":0,"step":"manifest"}"#,
                "\n",
                r#"{"ts":"20240103T0000"#,
            ),
        )
        .unwrap();
        let attempts = read_attempts(root);
        assert_eq!(
            attempts.len(),
            2,
            "the torn line must be skipped, not fatal"
        );
        assert!(!attempts[0].failed());
        assert!(attempts[1].failed());
        assert_eq!(attempts[1].step, "manifest");
    }

    #[test]
    fn a_missing_attempt_log_is_an_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_attempts(dir.path()).is_empty());
    }

    #[test]
    fn a_log_longer_than_the_tail_window_still_reports_the_newest_records() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Far more than the read window, so the oldest records are genuinely
        // outside it rather than merely many.
        let mut log = String::new();
        for index in 0..20_000 {
            log.push_str(&format!(
                r#"{{"ts":"t{index}","outcome":"error","set":null,"databases":0,"step":"export","filler":"{index}"}}"# 
            ));
            log.push('\n');
        }
        std::fs::write(root.join("attempts.jsonl"), &log).unwrap();
        let attempts = read_attempts(root);
        assert!(
            !attempts.is_empty(),
            "the tail window must not read as empty"
        );
        assert_eq!(
            attempts.last().unwrap().ts,
            "t19999",
            "the newest attempt must always be in the window"
        );
        assert!(
            attempts.len() < 20_000,
            "the read is supposed to be bounded, not a full scan"
        );
    }

    #[test]
    fn snapshot_reports_the_last_attempt_and_the_last_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        std::fs::create_dir_all(dir.path().join("dg-backups")).unwrap();
        std::fs::write(
            dir.path().join("dg-backups/attempts.jsonl"),
            concat!(
                r#"{"ts":"a","outcome":"ok","set":"a","step":"publish"}"#,
                "\n",
                r#"{"ts":"b","outcome":"error","set":null,"step":"export"}"#,
                "\n",
                r#"{"ts":"c","outcome":"ok","set":"c","step":"publish"}"#,
                "\n",
            ),
        )
        .unwrap();
        let health = snapshot(&store).unwrap();
        assert_eq!(health.last_attempt.as_ref().unwrap().ts, "c");
        assert_eq!(health.last_failure.as_ref().unwrap().ts, "b");
        assert_eq!(health.last_failure.as_ref().unwrap().step, "export");
        // A failure that has since been followed by a success is history, not
        // a current condition: the digest reports both and the immediate
        // notice has already fired.
        assert!(health.last_attempt.as_ref().unwrap().ts.as_str() > "b");
    }

    #[test]
    fn a_set_without_a_containing_database_is_unknown_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_backup_root(dir.path());
        store.create_and_note("alpha").unwrap();
        store.create_and_note("beta").unwrap();
        let root = dir.path().join("dg-backups");
        let boundary = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        write_set(&root, "20240101T000000Z", &["alpha"], Some(boundary));
        let health = snapshot(&store).unwrap();
        let beta = health
            .databases
            .iter()
            .find(|db| db.name == "beta")
            .expect("beta is live and must be reported");
        assert_eq!(beta.state, "unknown");
    }
}
