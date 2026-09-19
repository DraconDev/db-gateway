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
    fleet.cli.q("seed", "tables_db", "CREATE TABLE t (id INTEGER, name TEXT)");
    fleet.cli.q("insert", "tables_db", "INSERT INTO t VALUES (1, 'ada'), (2, 'bob')");

    // Piped: parses as JSON; identical to --json.
    let piped = fleet.cli.ok("query", &["query", "tables_db", "SELECT id, name FROM t ORDER BY id"]);
    assert_eq!(
        piped["rows"].as_array().unwrap().len(),
        2,
        "piped query not JSON rows: {piped}"
    );
    assert!(
        piped.to_string().starts_with("{\"columns\":[\"id\",\"name\"]"),
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
    assert!(!screen.contains("\"rows\""), "JSON leaked to tty: {screen:?}");
}
