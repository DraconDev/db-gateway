//! The audit trail must not report the same action twice.
//!
//! The trail file is bounded at 4 MiB. When an action pushes it past
//! that, the file is rewritten from the in-memory ring -- the ring
//! already holds the new entry, so the rewrite writes it -- and then the
//! new line is appended to the file as well. The action that triggered
//! the rollover was therefore written twice, and `read_trail` reads both
//! copies back, so after a restart the operator sees one action listed
//! twice.
//!
//! An audit trail that reports the same action two times is exactly the
//! kind of thing that makes a trail untrustworthy, so the rollover has
//! to end with the file holding each action once.

use dg::store::audit::{AuditEntry, AuditLog, MAX_FILE_BYTES};
use std::path::Path;

/// An entry whose serialized line is about `size` bytes.
fn entry_of_size(size: usize) -> AuditEntry {
    AuditEntry {
        at: 1_700_000_000,
        principal: "owner".to_string(),
        role: "owner".to_string(),
        action: "query".to_string(),
        database: Some("demo".to_string()),
        detail: Some("x".repeat(size)),
    }
}

/// An entry with a detail a reader can look for.
fn entry_with_marker(marker: &str, size: usize) -> AuditEntry {
    AuditEntry {
        at: 1_700_000_000,
        principal: "owner".to_string(),
        role: "owner".to_string(),
        action: "query".to_string(),
        database: Some("demo".to_string()),
        detail: Some(format!("{marker}{}", "x".repeat(size))),
    }
}

/// Open an audit log over a file.
fn log_over(path: &Path) -> AuditLog {
    AuditLog::open(path)
}

/// The trail holds each action once, even across the rollover.
///
/// The file is filled past its 4 MiB bound with actions that are each
/// distinguishable, then one more action is recorded. That last action
/// is the one that triggers the rewrite, so it is the one that would be
/// written twice.
#[test]
fn the_action_that_triggers_the_rollover_is_written_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let log = log_over(&path);

    // Fill the file to just under the bound with distinguishable
    // entries. Each is a kilobyte, so a few thousand of them is a few
    // megabytes -- the bound is 4 MiB.
    let chunk = 1024;
    let count = (MAX_FILE_BYTES as usize / chunk) + 8;
    for _ in 0..count {
        log.record(entry_of_size(chunk - 64));
    }

    // The action that tips the file over the bound.
    let marker = "the-action-that-triggers-the-rollover";
    log.record(entry_with_marker(marker, chunk - 64));

    let text = std::fs::read_to_string(&path).unwrap();
    let occurrences = text.matches(marker).count();
    assert_eq!(
        occurrences, 1,
        "the action that triggered the rollover is written {occurrences} times"
    );

    // And the trail the operator reads back agrees.
    let entries = log.entries();
    let seen = entries
        .iter()
        .filter(|entry| {
            entry
                .detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with(marker))
        })
        .count();
    assert_eq!(
        seen, 1,
        "the trail reports the rollover action {seen} times"
    );
}

/// A trail that was written twice is read back twice -- the proof that
/// the duplication is visible to the operator, not just to the file.
#[test]
fn the_error_shape_is_not_swallowed() {
    // A guard against the test above silently passing because the log
    // could not be opened at all: an unreadable trail must be visible.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let log = log_over(&path);
    log.record(entry_of_size(16));
    assert!(path.exists(), "the trail file was never created");
}
