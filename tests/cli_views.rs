//! Contract tests for the operator views: `dg status`, `dg watch` and
//! `dg inspect`.
//!
//! The contract these protect is that a view ANSWERS a question. A
//! dashboard that lists the server's properties is not informative; one
//! that says how many databases are live, how busy the service is, how
//! often it fails and which database is doing the work is.
//!
//! Run: `timeout 300 cargo test --locked --test cli_views`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::process::Command;

/// Strip ANSI escapes so a text assertion sees the words, not the
/// colour codes around them.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

/// Run a command under a pty so the CLI takes its terminal path.
fn pty(fleet: &Fleet, args: &str) -> String {
    let out = Command::new("script")
        .args([
            "-qec",
            &format!(
                "{} --server {} --key-file {} {args}",
                env!("CARGO_BIN_EXE_dg"),
                fleet.cli.server,
                fleet.cli.key_file.to_str().unwrap(),
            ),
            "/dev/null",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "pty `{args}` failed: {out:?}");
    plain(&String::from_utf8_lossy(&out.stdout))
}

#[test]
fn the_status_view_answers_the_operators_questions() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "view_db"]);
    fleet
        .cli
        .q("seed", "view_db", "CREATE TABLE t (id INTEGER)");
    fleet
        .cli
        .q("insert", "view_db", "INSERT INTO t VALUES (1), (2)");

    let screen = pty(&fleet, "status");

    // How many databases are there, and how many are on their way out.
    assert!(
        screen.contains("1 live, 0 tombstoned"),
        "no live/tombstoned counts: {screen}"
    );
    // How busy is it, and how often does it fail.
    assert!(screen.contains("requests"), "no request total: {screen}");
    assert!(screen.contains("rate"), "no request rate: {screen}");
    assert!(screen.contains("error rate"), "no error rate: {screen}");
    // The per-database view: which database is doing the work.
    assert!(
        screen.contains("per-database"),
        "no per-database section: {screen}"
    );
    assert!(
        screen.contains("view_db"),
        "the database is not in the table: {screen}"
    );
    for column in ["rows read", "rows written", "waits", "last write"] {
        assert!(
            screen.contains(column),
            "the per-database table has no `{column}` column: {screen}"
        );
    }
}

#[test]
fn the_watch_view_is_the_status_view_read_once() {
    // `dg watch` is a snapshot: it prints the same dashboard and
    // exits, so it stays pipeable and diffable.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "watch_db"]);

    let status = pty(&fleet, "status");
    let watch = pty(&fleet, "watch");

    for section in ["service", "activity", "admission"] {
        assert!(
            status.contains(section),
            "dg status has no `{section}` section: {status}"
        );
        assert!(
            watch.contains(section),
            "dg watch has no `{section}` section: {watch}"
        );
    }
    assert!(
        watch.contains("1 live, 0 tombstoned"),
        "dg watch does not show the database counts: {watch}"
    );
}

#[test]
fn the_inspect_view_shows_one_database_and_what_it_did() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "inspect_db"]);
    fleet
        .cli
        .q("seed", "inspect_db", "CREATE TABLE t (id INTEGER)");
    fleet
        .cli
        .q("insert", "inspect_db", "INSERT INTO t VALUES (1), (2)");

    let screen = pty(&fleet, "inspect inspect_db");

    // What the database IS.
    assert!(screen.contains("inspect_db"), "no name: {screen}");
    assert!(screen.contains("live"), "no state: {screen}");
    assert!(screen.contains("size"), "no size: {screen}");
    // What it has been DOING.
    assert!(screen.contains("activity"), "no activity section: {screen}");
    assert!(screen.contains("rows written"), "no rows written: {screen}");
    assert!(screen.contains("queue waits"), "no queue waits: {screen}");
    assert!(screen.contains("last write"), "no last write: {screen}");
    // How its backup is doing.
    assert!(screen.contains("backup"), "no backup section: {screen}");
    // What it holds.
    assert!(screen.contains("tables"), "no tables section: {screen}");
    assert!(screen.contains("t"), "the table is not listed: {screen}");
}

#[test]
fn the_views_stay_machine_readable_when_piped() {
    // The views are for people, but a script still has to be able to
    // read the same facts. Piped output stays JSON.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "machine_db"]);

    for args in ["status", "watch", "inspect machine_db"] {
        let piped = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args([
                "--server",
                &fleet.cli.server,
                "--key-file",
                fleet.cli.key_file.to_str().unwrap(),
            ])
            .args(args.split(' '))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&piped.stdout).to_string();
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_ok(),
            "piped `{args}` is not JSON: {text}"
        );
    }
}

#[test]
fn a_tombstoned_database_is_still_counted_and_still_inspectable() {
    // Tombstoning is not deleting: the name is taken until the
    // retention window purges it, and the operator has to be able to
    // see that.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "doomed_db"]);
    let (ok, _stdout, _stderr) = fleet
        .cli
        .run(&["delete", "doomed_db", "--confirm", "doomed_db"]);
    assert!(ok, "delete failed");

    let screen = pty(&fleet, "status");
    assert!(
        screen.contains("0 live, 1 tombstoned"),
        "the tombstone is not counted: {screen}"
    );

    let screen = pty(&fleet, "inspect doomed_db");
    assert!(
        screen.contains("tombstoned"),
        "the state does not say tombstoned: {screen}"
    );
}
