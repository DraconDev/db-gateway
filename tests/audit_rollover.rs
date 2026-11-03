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

use dg::store::{AuditEntry, AuditLog};
use std::path::Path;

/// The bound the trail file is rolled over at.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// An entry with a detail a reader can look for.
fn entry_with_marker(marker: &str) -> AuditEntry {
    AuditEntry {
        at: 1_700_000_000,
        principal: "owner".to_string(),
        role: "owner".to_string(),
        action: "query".to_string(),
        database: Some("demo".to_string()),
        detail: Some(marker.to_string()),
        elapsed_us: None,
    }
}

/// The entry that crosses the bound: a marker plus enough detail to
/// push the file past it on its own.
fn the_rollover_entry(marker: &str) -> AuditEntry {
    let mut entry = entry_with_marker(marker);
    let detail = entry.detail.take().unwrap_or_default();
    entry.detail = Some(format!("{detail}{}", "x".repeat(4096)));
    entry
}

/// Fill the trail file until it is one entry short of the rollover.
///
/// The entries are ordinary ones -- a principal and a statement shape,
/// which is what the trail really holds -- so the file grows one line at
/// a time and the test can stop just below the bound.
fn fill_to_the_bound(path: &Path, log: &AuditLog) {
    let mut index = 0usize;
    loop {
        let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        // Stop with room for the rollover entry to cross the bound on
        // its own: a filler must not be the one that triggers it.
        if size + 1024 >= MAX_FILE_BYTES {
            return;
        }
        log.record(entry_with_marker(&format!("filler-{index}")));
        index += 1;
        assert!(index < 200_000, "the file never filled up");
    }
}

/// The action that triggers the rollover is written once.
#[test]
fn the_action_that_triggers_the_rollover_is_written_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let log = AuditLog::open(&path);
    fill_to_the_bound(&path, &log);

    // The action that tips the file over the bound.
    let marker = "the-action-that-triggers-the-rollover";
    log.record(the_rollover_entry(marker));

    let text = std::fs::read_to_string(&path).unwrap();
    let occurrences = text.matches(marker).count();
    assert_eq!(
        occurrences, 1,
        "the action that triggered the rollover is written {occurrences} times"
    );

    // And the trail the operator reads back agrees.
    let seen = log
        .entries()
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

/// The trail a restarted gateway reads back holds each action once.
///
/// The file is the durable record, so the duplication the operator sees
/// after a restart is the one that matters: `read_trail` reads every
/// line back, including the line that was written twice.
#[test]
fn a_restarted_gateway_reads_the_action_back_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let marker = "the-action-that-triggers-the-rollover";

    {
        let log = AuditLog::open(&path);
        fill_to_the_bound(&path, &log);
        log.record(the_rollover_entry(marker));
    }

    // A fresh log over the same file is what a restarted gateway builds.
    let reopened = AuditLog::open(&path);
    let seen = reopened
        .entries()
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
        "a restarted gateway reads the rollover action back {seen} times"
    );
}
