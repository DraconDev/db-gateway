//! Contract tests for the new commands: `dg key tokens`, `dg execute`
//! and `dg inspect`.
//!
//! `dg key tokens` answers "who can touch this database", which is the
//! first question of a leaked-key incident. `dg execute` runs a SQL
//! file, and the file's statements have to be found correctly -- a
//! semicolon inside a string is not a statement boundary.
//!
//! Run: `timeout 300 cargo test --locked --test cli_commands`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::process::Command;

/// Strip ANSI escapes so a text assertion sees the words.
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

/// Run a command and return its stdout, whatever the exit code.
fn run(fleet: &Fleet, args: &[&str]) -> (bool, String, String) {
    fleet.cli.run(args)
}

#[test]
fn key_tokens_lists_the_credentials_that_can_reach_a_database() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "reached_db"]);
    fleet.cli.ok("create", &["create", "other_db"]);
    fleet
        .cli
        .ok("issue", &["key", "create", "bot", "--role", "database_read_only", "--database", "reached_db"]);

    let screen = pty(&fleet, "key tokens reached_db");
    // The scoped key reaches the database it was scoped to.
    assert!(screen.contains("bot"), "the scoped key is missing: {screen}");
    assert!(
        screen.contains("reached_db"),
        "the scope is missing: {screen}"
    );
    // The owner key reaches everything.
    assert!(
        screen.contains("owner"),
        "the owner key is missing: {screen}"
    );

    // A key scoped to another database does NOT reach this one.
    let screen = pty(&fleet, "key tokens other_db");
    assert!(
        !screen.contains("bot"),
        "the scoped key reached the wrong database: {screen}"
    );
    assert!(
        screen.contains("owner"),
        "the owner key is missing: {screen}"
    );
}

#[test]
fn key_tokens_stays_machine_readable_when_piped() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "piped_db"]);
    let (ok, stdout, stderr) = run(&fleet, &["--json", "key", "tokens", "piped_db"]);
    assert!(ok, "failed: {stderr}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
        "not JSON: {stdout}"
    );
}

#[test]
fn execute_runs_every_statement_of_a_script_and_reports_each_one() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "script_db"]);

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.sql");
    std::fs::write(
        &script,
        // A comment, a semicolon inside a string, and a semicolon in a
        // comment: none of them is a statement boundary.
        "-- set up\n\
         CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT, note TEXT);\n\
         INSERT INTO players VALUES (1, 'aria', 'a;b');\n\
         -- a trailing comment with a semicolon ;\n\
         INSERT INTO players VALUES (2, 'octavia', 'plain');\n",
    )
    .unwrap();

    let screen = pty(
        &fleet,
        &format!(
            "execute script_db {}",
            script.to_str().unwrap()
        ),
    );
    // Every statement is reported, in order.
    assert!(screen.contains("CREATE TABLE"), "no create: {screen}");
    assert!(screen.contains("INSERT"), "no insert: {screen}");
    assert_eq!(
        screen.matches("INSERT").count(),
        2,
        "both inserts are missing: {screen}"
    );

    // The rows landed, and the semicolon inside the string survived as
    // data rather than splitting the statement.
    let rows = fleet.cli.q(
        "read back",
        "script_db",
        "SELECT id, name, note FROM players ORDER BY id",
    );
    assert_eq!(rows[0][2], "a;b", "the string was split: {rows:?}");
    assert_eq!(rows[1][2], "plain", "the string was split: {rows:?}");
    assert_eq!(rows[1][1], "octavia");
}

#[test]
fn execute_refuses_a_script_with_no_statements() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "empty_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("empty.sql");
    std::fs::write(&script, "-- nothing but a comment\n").unwrap();
    let (ok, _stdout, stderr) = run(
        &fleet,
        &["execute", "empty_db", script.to_str().unwrap()],
    );
    assert!(!ok, "an empty script was accepted");
    assert!(
        stderr.contains("empty_script") || stderr.contains("no statements"),
        "the refusal does not say why: {stderr}"
    );
}

#[test]
fn execute_reports_a_failing_statement() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "failing_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("bad.sql");
    // The second statement cannot run: the table does not exist.
    std::fs::write(
        &script,
        "SELECT * FROM no_such_table;\nINSERT INTO other VALUES (1);\n",
    )
    .unwrap();
    let (ok, stdout, stderr) = run(
        &fleet,
        &["execute", "failing_db", script.to_str().unwrap()],
    );
    assert!(!ok, "a failing script was accepted: {stdout}");
    assert!(
        stdout.contains("no_such_table") || stderr.contains("no_such_table"),
        "the failure is not named: {stdout} {stderr}"
    );
}

#[test]
fn execute_stays_machine_readable_when_piped() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "piped_script_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.sql");
    std::fs::write(&script, "CREATE TABLE t (id INTEGER);\n").unwrap();
    let (ok, stdout, stderr) = run(
        &fleet,
        &["execute", "piped_script_db", script.to_str().unwrap()],
    );
    assert!(ok, "failed: {stderr}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
        "not JSON: {stdout}"
    );
}
