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

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

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

/// The whole snapshot: what the digest reports and what the endpoint serves.
#[derive(Debug, Clone, Serialize)]
pub struct BackupHealth {
    pub backup_root: Option<PathBuf>,
    pub stale_after_secs: u64,
    pub generated_at: u64,
    pub databases: Vec<DatabaseHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<AttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<AttemptRecord>,
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
    let mut names: Vec<String> = store.list_all()?;
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
            if selected.coverage_start.is_none()
                && sets
                    .iter()
                    .filter(|set| set.databases.contains(name))
                    .any(|set| set.mtime > selected.mtime)
            {
                return None;
            }
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
    Ok(BackupHealth {
        backup_root: root,
        stale_after_secs: STALE_AFTER_SECS,
        generated_at: now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        databases,
        last_attempt,
        last_failure,
    })
}

/// Read the producer's attempt log. Tolerant by construction: the file is
/// appended to by a separate process while a backup runs, so a torn final
/// line is normal, not corruption, and a digest must not fail to render
/// because the newest line is half-written. Unparseable lines are skipped,
/// and a missing log is an empty log -- "no attempts recorded yet" is a
/// reportable state, not an error.
pub fn read_attempts(root: &std::path::Path) -> Vec<AttemptRecord> {
    let path = root.join("attempts.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<AttemptRecord>(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    fn age_of(root: &std::path::Path, stamp: &str) -> SystemTime {
        // The freshness age is measured from the set's mtime; a test that
        // wanted to assert a number would have to sleep, so instead the
        // tests assert state and the newest set, and only that an age exists.
        root.join(stamp).metadata().unwrap().modified().unwrap()
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
        let _ = age_of(&root, "20240101T000000Z");
    }
}
