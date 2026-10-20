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
    for column in [
        "rows read",
        "rows written",
        "waits",
        "wait p95",
        "wait max",
        "last write",
    ] {
        assert!(
            screen.contains(column),
            "the per-database table has no `{column}` column: {screen}"
        );
    }
}

#[test]
fn the_per_database_wait_columns_name_what_they_hold() {
    // The wait-latency column used to hold the database's MAXIMUM wait
    // under a "wait p95" header, which is a wrong number under a false
    // name: on a contended database the max overstates the p95 and
    // disagrees with the genuine p95 in the admission block of the same
    // screen. The columns must say which is which, and the p95 column
    // must hold the server's p95.
    //
    // The assertion is PER COLUMN, not per row: with one wait a
    // database's p95 and its max are the same number, so a check that
    // looks for the p95 anywhere in the row passes even when the two
    // cells are swapped. The renderer-level test in src/render.rs
    // pins the two cells apart with values that differ; this one
    // checks the same wiring end to end.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "wait_db"]);
    fleet
        .cli
        .q("seed", "wait_db", "CREATE TABLE t (id INTEGER)");

    // A contended database, so its p95 and its max are DIFFERENT
    // numbers. With a single wait they are the same, and a swapped
    // pair of cells would look correct.
    let mut readers = Vec::new();
    for _ in 0..16 {
        let cli = &fleet.cli;
        readers.push(std::thread::spawn(move || {
            let _ = cli.run(&["query", "wait_db", "SELECT count(*) FROM t"]);
        }));
    }
    for reader in readers {
        reader.join().unwrap();
    }

    let screen = pty(&fleet, "status");
    // The header names the two different facts.
    assert!(
        screen.contains("wait p95"),
        "the per-database table has no wait p95 column: {screen}"
    );
    assert!(
        screen.contains("wait max"),
        "the per-database table has no wait max column: {screen}"
    );

    // The rendered values are the server's own: the p95 column holds
    // the p95, and the max column holds the max. Reading them from the
    // JSON is what makes this a check of the RENDERING rather than a
    // check that some digits appear.
    let (ok, stdout, _) = fleet.cli.run(&["--json", "status"]);
    assert!(ok, "status failed: {stdout}");
    let status: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    let entry = &status["databases"]["wait_db"];
    let p95 = entry["queue_wait_p95_us"].as_u64();
    let max = entry["queue_wait_max_us"].as_u64();
    assert!(
        p95.is_some() && max.is_some(),
        "the server reported no wait measurements: {entry}"
    );
    let (p95, max) = (p95.unwrap(), max.unwrap());
    // The p95 is at or below the max, always: a percentile cannot
    // exceed the worst sample.
    assert!(p95 <= max, "the p95 {p95} exceeds the max {max}: {entry}");

    // Each cell, at its own column, holds its own value. The columns
    // are located by their headers, so the check survives a change to
    // the column order.
    let header: Vec<String> = screen
        .lines()
        .find(|line| line.contains("wait p95") && line.contains("wait max"))
        .unwrap_or_else(|| panic!("no per-database header: {screen}"))
        .split('│')
        .map(|cell| cell.trim().to_string())
        .filter(|cell| !cell.is_empty())
        .collect();
    let row: Vec<String> = screen
        .lines()
        .find(|line| line.contains("wait_db") && line.contains('│'))
        .unwrap_or_else(|| panic!("the database is not in the table: {screen}"))
        .split('│')
        .map(|cell| cell.trim().to_string())
        .filter(|cell| !cell.is_empty())
        .collect();
    let p95_column = header
        .iter()
        .position(|cell| cell == "wait p95")
        .expect("no wait p95 column");
    let max_column = header
        .iter()
        .position(|cell| cell == "wait max")
        .expect("no wait max column");
    assert!(
        row[p95_column].contains(&p95.to_string()),
        "the wait p95 column does not hold the p95 {p95}: {row:?}"
    );
    assert!(
        row[max_column].contains(&max.to_string()),
        "the wait max column does not hold the max {max}: {row:?}"
    );
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
