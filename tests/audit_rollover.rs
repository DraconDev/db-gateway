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

/// An entry with a detail a reader can look for, and a total line size
/// of roughly `size` bytes.
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

/// The action that triggers the rollover is written once.
///
/// The file is filled past its 4 MiB bound with entries big enough that
/// the ring's own cap does not keep the file small -- the ring holds
/// 2000 entries, so 4 KiB entries reach the bound -- and then one more
/// action is recorded. That last action is the one that tips the file
/// over, so it is the one that would be written twice.
#[test]
fn the_action_that_triggers_the_rollover_is_written_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let log = AuditLog::open(&path);

    // Fill past the bound with ordinary entries -- a statement shape
    // and a principal, which is what the trail really holds. The ring
    // keeps only the newest 2000, so the FILE grows well past the bound
    // while the ring stays small, which is the situation the rollover
    // exists for.
    let chunk = 200;
    for index in 0..24_000 {
        log.record(entry_with_marker(&format!("filler-{index}-"), chunk - 64));
    }
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(
        size > MAX_FILE_BYTES,
        "the file never reached the rollover bound: {size} bytes"
    );

    // The action that tips the file over the bound.
    let marker = "the-action-that-triggers-the-rollover";
    log.record(entry_with_marker(marker, chunk - 128));

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

/// The trail a restarted gateway reads back holds each action once.
///
/// The file is the durable record, so the duplication the operator sees
/// after a restart is the one that matters: `read_trail` reads every
/// line back, including the line that was written twice.
#[test]
fn a_restarted_gateway_reads_the_action_back_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");

    {
        let log = AuditLog::open(&path);
        let chunk = 200;
        for index in 0..24_000 {
            log.record(entry_with_marker(&format!("filler-{index}-"), chunk - 64));
        }
        let marker = "the-action-that-triggers-the-rollover";
        log.record(entry_with_marker(marker, chunk - 128));
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
                .is_some_and(|detail| detail.starts_with("the-action-that-triggers-the-rollover"))
        })
        .count();
    assert_eq!(
        seen, 1,
        "a restarted gateway reads the rollover action back {seen} times"
    );
}
