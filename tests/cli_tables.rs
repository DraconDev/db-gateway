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

    // Pty: aligned table, no JSON envelope.
    let inner = format!(
        "{} --server {} --key-file {} query tables_db \"SELECT id, name FROM t ORDER BY id\"",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let pty = Command::new("script")
        .args(["-qec", &inner, "/dev/null"])
        .output()
        .unwrap();
    assert!(pty.status.success(), "pty query failed: {pty:?}");
    let screen = String::from_utf8_lossy(&pty.stdout).into_owned();
    assert!(screen.contains("id | name"), "no table header: {screen:?}");
    assert!(screen.contains("1  | ada"), "no table row: {screen:?}");
    assert!(
        !screen.contains("\"rows\""),
        "JSON leaked to tty: {screen:?}"
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
    assert!(screen.contains("name"), "no list header: {screen:?}");
    assert!(screen.contains("tables_cli"), "no list row: {screen:?}");
    assert!(
        !screen.contains("\"databases\""),
        "JSON leaked to tty: {screen:?}"
    );

    let info = Command::new("script")
        .args(["-qec", &format!("{base} info tables_cli"), "/dev/null"])
        .output()
        .unwrap();
    assert!(info.status.success(), "pty info failed: {info:?}");
    let screen = String::from_utf8_lossy(&info.stdout).into_owned();
    for (label, needle) in [
        ("header", "property"),
        ("header value column", "value"),
        ("name row", "tables_cli"),
        ("usage row", "rows_read"),
        ("usage value", "rows_written | 1"),
        ("tables row", "t (1 rows)"),
    ] {
        assert!(screen.contains(needle), "no {label}: {screen:?}");
    }
    assert!(
        !screen.contains("\"tables\""),
        "JSON leaked to tty: {screen:?}"
    );
}

#[test]
fn status_backup_keys_piped_json_pty_table() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "tables_status"]);
    fleet.cli.q(
        "seed",
        "tables_status",
        "CREATE TABLE t (id INTEGER)",
    );

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
        piped_keys["keys"].as_array().unwrap().iter().any(|k| k["name"] == "owner"),
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
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let screen = run_pty("status");
    for (label, needle) in [
        ("header", "property | value"),
        ("version row", "version"),
        ("usage row", "usage tables_status"),
        ("queue waits", "queue"),
        ("queue timeouts", "timeouts"),
    ] {
        assert!(screen.contains(needle), "no {label}: {screen:?}");
    }
    assert!(
        !screen.contains("\"uptime_secs\""),
        "JSON leaked to tty: {screen:?}"
    );

    let screen = run_pty("backup status");
    assert!(screen.contains("backup_root"), "no root row: {screen:?}");
    assert!(
        !screen.contains("\"backup_root\""),
        "JSON leaked to tty: {screen:?}"
    );

    let screen = run_pty("key list");
    assert!(
        screen.contains("name | role | databases"),
        "no key header: {screen:?}"
    );
    assert!(screen.contains("owner"), "no owner row: {screen:?}");
    assert!(
        !screen.contains("\"keys\""),
        "JSON leaked to tty: {screen:?}"
    );
}
