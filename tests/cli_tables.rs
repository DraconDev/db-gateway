//! Auto-tty table output for `dg query` (CLI only).
//!
//! Run: `timeout 300 cargo test --locked --test cli_tables`
//!
//! Proves the contract through the real `dg` binary: piped `query`
//! output parses as JSON byte-for-byte identical to `--json` output,
//! while the same command under a pty renders the shell's aligned
//! table. Zero direct HTTP; the pty leg uses `script(1)`.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::process::Command;

/// Strip ANSI escapes so a terminal's colouring does not break a text
/// assertion. The colour itself is asserted separately: the pty leg
/// proves the styled output is actually styled.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // A CSI sequence is ESC '[' params final. The '[' is the
        // introducer, not the terminator, so it is consumed explicitly
        // before the final byte is looked for; treating it as the
        // terminator would leave the parameter bytes in the text.
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

#[test]
fn query_piped_json_pty_table() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "tables_db"]);
    fleet.cli.q(
        "seed",
        "tables_db",
        "CREATE TABLE t (id INTEGER, name TEXT)",
    );
    fleet.cli.q(
        "insert",
        "tables_db",
        "INSERT INTO t VALUES (1, 'ada'), (2, 'bob')",
    );

    // Piped: parses as JSON; identical to --json.
    let piped = fleet.cli.ok(
        "query",
        &["query", "tables_db", "SELECT id, name FROM t ORDER BY id"],
    );
    assert_eq!(
        piped["rows"].as_array().unwrap().len(),
        2,
        "piped query not JSON rows: {piped}"
    );
    assert!(
        piped
            .to_string()
            .starts_with("{\"columns\":[\"id\",\"name\"]"),
        "piped query not JSON: {piped}"
    );
    let forced = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            &fleet.cli.server,
            "--key-file",
            fleet.cli.key_file.to_str().unwrap(),
            "--json",
            "query",
            "tables_db",
            "SELECT id, name FROM t ORDER BY id",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&forced.stdout).trim(),
        piped.to_string(),
        "--json differs from piped default"
    );

    // Pty: the table, no JSON envelope.
    //
    // The pty leg runs with an EXPLICIT environment, so the colour
    // assertion does not depend on the machine the test runs on: a CI
    // runner with NO_COLOR set, or a TERM=dumb terminal, would otherwise
    // fail a test that is about the table, not about the runner's
    // environment. The colour-off behaviour is asserted separately, in
    // tests/cli_pretty.rs, against the documented rules.
    let inner = format!(
        "{} --server {} --key-file {} query tables_db \"SELECT id, name FROM t ORDER BY id\"",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let pty = Command::new("script")
        .env_remove("NO_COLOR")
        .env("TERM", "xterm-256color")
        .args(["-qec", &inner, "/dev/null"])
        .output()
        .unwrap();
    assert!(pty.status.success(), "pty query failed: {pty:?}");
    let screen = String::from_utf8_lossy(&pty.stdout).into_owned();
    let text = plain(&screen);
    assert!(text.contains("│ id │ name │"), "no table header: {text:?}");
    assert!(text.contains("│  1 │ ada"), "no table row: {text:?}");
    assert!(!text.contains("\"rows\""), "JSON leaked to tty: {text:?}");
    // A terminal gets the styled table, not the plain one.
    assert!(
        screen.contains("\u{1b}["),
        "no colour in the terminal table: {screen:?}"
    );
}

#[test]
fn list_info_piped_json_pty_table() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "tables_cli"]);
    fleet
        .cli
        .q("seed", "tables_cli", "CREATE TABLE t (id INTEGER)");
    fleet
        .cli
        .q("insert", "tables_cli", "INSERT INTO t VALUES (1)");

    // Piped legs stay machine-readable JSON.
    let piped_list = fleet.cli.ok("list", &["list"]);
    assert!(
        piped_list["databases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "tables_cli"),
        "piped list not JSON names: {piped_list}"
    );
    let piped_info = fleet.cli.ok("info", &["info", "tables_cli"]);
    assert_eq!(piped_info["name"], "tables_cli");
    assert_eq!(piped_info["tombstoned"], false);

    // Pty legs render aligned tables with no JSON envelope.
    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let list = Command::new("script")
        .args(["-qec", &format!("{base} list"), "/dev/null"])
        .output()
        .unwrap();
    assert!(list.status.success(), "pty list failed: {list:?}");
    let screen = String::from_utf8_lossy(&list.stdout).into_owned();
    let text = plain(&screen);
    assert!(text.contains("database"), "no list header: {text:?}");
    assert!(text.contains("tables_cli"), "no list row: {text:?}");
    assert!(
        !text.contains("\"databases\""),
        "JSON leaked to tty: {text:?}"
    );

    let info = Command::new("script")
        .args(["-qec", &format!("{base} info tables_cli"), "/dev/null"])
        .output()
        .unwrap();
    assert!(info.status.success(), "pty info failed: {info:?}");
    let screen = String::from_utf8_lossy(&info.stdout).into_owned();
    let text = plain(&screen);
    for (label, needle) in [
        ("name row", "tables_cli"),
        ("size row", "KiB"),
        ("usage row", "rows read"),
        ("usage value", "rows written"),
        ("tables row", "t"),
    ] {
        assert!(text.contains(needle), "no {label}: {text:?}");
    }
    assert!(!text.contains("\"tables\""), "JSON leaked to tty: {text:?}");
}

#[test]
fn status_backup_keys_piped_json_pty_table() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "tables_status"]);
    fleet
        .cli
        .q("seed", "tables_status", "CREATE TABLE t (id INTEGER)");

    // Piped legs stay machine-readable JSON.
    let piped_status = fleet.cli.ok("status", &["status"]);
    assert!(piped_status["version"].is_string());
    assert!(piped_status["databases"]["tables_status"]["queue_waits"].is_number());
    let piped_backups = fleet.cli.ok("backup-status", &["backup", "status"]);
    assert!(
        piped_backups["backup_root"].is_string() || piped_backups["backup_root"].is_null(),
        "piped backup status not JSON: {piped_backups}"
    );
    let piped_keys = fleet.cli.ok("key-list", &["key", "list"]);
    assert!(
        piped_keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["name"] == "owner"),
        "piped key list not JSON keys: {piped_keys}"
    );

    // Pty legs render aligned tables with no JSON envelope.
    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let run_pty = |args: &str| {
        let out = Command::new("script")
            .args(["-qec", &format!("{base} {args}"), "/dev/null"])
            .output()
            .unwrap();
        assert!(out.status.success(), "pty {args} failed: {out:?}");
        plain(&String::from_utf8_lossy(&out.stdout))
    };

    let screen = run_pty("status");
    for (label, needle) in [
        ("service section", "service"),
        ("version row", "version"),
        ("admission section", "admission"),
        ("requests section", "requests"),
        ("per-database section", "per-database"),
        ("database row", "tables_status"),
        ("queue waits column", "waits"),
    ] {
        assert!(screen.contains(needle), "no {label}: {screen:?}");
    }
    assert!(
        !screen.contains("\"uptime_secs\""),
        "JSON leaked to tty: {screen:?}"
    );

    let screen = run_pty("backup status");
    assert!(screen.contains("backup"), "no root section: {screen:?}");
    assert!(screen.contains("root"), "no root row: {screen:?}");
    assert!(
        !screen.contains("\"backup_root\""),
        "JSON leaked to tty: {screen:?}"
    );

    let screen = run_pty("key list");
    for (label, needle) in [
        ("header name column", "name"),
        ("header role column", "role"),
        ("header databases column", "databases"),
        ("owner row", "owner"),
    ] {
        assert!(screen.contains(needle), "no {label}: {screen:?}");
    }
    assert!(
        !screen.contains("\"keys\""),
        "JSON leaked to tty: {screen:?}"
    );
}
