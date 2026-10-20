//! Contract tests for the shell as the operator's control surface.
//!
//! The shell and the one-shot commands must draw the SAME view of the
//! SAME data: an operator who learns `dg status` should not have to
//! learn a second format inside `dg shell`.
//!
//! Run: `timeout 300 cargo test --locked --test shell_control`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::io::Write;
use std::process::{Command, Stdio};

/// Drive the shell with a script and return what it drew.
fn shell(fleet: &Fleet, script: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            &fleet.cli.server,
            "--key-file",
            fleet.cli.key_file.to_str().unwrap(),
            "shell",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_shell_has_dot_commands_for_the_operator_surface() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shell_db"]);

    // The help names them, so an operator can find them.
    let screen = shell(&fleet, ".help\n.quit\n");
    for command in [".status", ".keys", ".backups"] {
        assert!(
            screen.contains(command),
            "the help does not mention {command}: {screen}"
        );
    }
}

#[test]
fn the_shell_draws_the_same_status_dashboard_as_the_command() {
    // The same view of the same data: the shell and `dg status` must
    // not drift apart.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shell_db"]);

    let screen = shell(&fleet, ".status\n.quit\n");
    assert!(screen.contains("service"), "no service section: {screen}");
    assert!(screen.contains("activity"), "no activity section: {screen}");
    assert!(screen.contains("admission"), "no admission section: {screen}");
    assert!(
        screen.contains("live, 0 tombstoned"),
        "no database counts: {screen}"
    );
}

#[test]
fn the_shell_lists_the_credentials() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shell_db"]);
    fleet.cli.ok(
        "issue",
        &[
            "key",
            "create",
            "shell_bot",
            "--role",
            "database_read_only",
            "--database",
            "shell_db",
        ],
    );

    let screen = shell(&fleet, ".keys\n.quit\n");
    assert!(screen.contains("shell_bot"), "the key is missing: {screen}");
    assert!(screen.contains("owner"), "the owner key is missing: {screen}");
    // It is a table, not a JSON dump.
    assert!(
        screen.contains('│') || screen.contains('|'),
        "the keys are not drawn as a table: {screen}"
    );
    assert!(
        !screen.contains("\"keys\""),
        "the keys are drawn as raw JSON: {screen}"
    );
}

#[test]
fn the_shell_shows_the_backup_freshness() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shell_db"]);

    let screen = shell(&fleet, ".backups\n.quit\n");
    assert!(screen.contains("shell_db"), "the database is missing: {screen}");
    assert!(
        screen.contains("state") && screen.contains("newest set"),
        "the columns are missing: {screen}"
    );
}

#[test]
fn the_shell_still_draws_its_tables_and_queries() {
    // The new commands must not have broken the ones that were there.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shell_db"]);
    fleet
        .cli
        .q("seed", "shell_db", "CREATE TABLE t (id INTEGER, name TEXT)");
    fleet
        .cli
        .q("insert", "shell_db", "INSERT INTO t VALUES (1, 'ada')");

    let screen = shell(&fleet, "use shell_db\n.tables\nSELECT * FROM t\n.quit\n");
    assert!(screen.contains("t"), "the table is not listed: {screen}");
    assert!(screen.contains("ada"), "the row is not shown: {screen}");
    assert!(
        screen.contains("using shell_db"),
        "the database switch is not confirmed: {screen}"
    );
}
