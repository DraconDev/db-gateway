//! `dg info`: size, tables, creation, tombstone state (CLI only).
//!
//! Run: `timeout 300 cargo test --locked --test cli_info`
//!
//! Exercises the info route through the real `dg` binary only: exact
//! tables with row counts on a seeded database, `_dg_*` internals
//! hidden, size positive, created_at inside the creation window,
//! 404 on unknown names, tombstone state after delete, and owner-only
//! rejection for scoped keys. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn info_reports_exact_database_facts() {
    let fleet = Fleet::boot();
    let before = epoch();
    assert_eq!(fleet.cli.ok("create", &["create", "info_db"])["name"], "info_db");
    let after_create = epoch();
    fleet.cli.q(
        "ddl",
        "info_db",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)",
    );
    fleet.cli.q("seed", "info_db", "INSERT INTO t(v) VALUES ('a'), ('b'), ('c')");
    fleet.cli.q("ddl2", "info_db", "CREATE TABLE u(id INTEGER PRIMARY KEY)");

    let info = fleet.cli.ok("info", &["info", "info_db"]);
    assert_eq!(info["name"], "info_db");
    assert_eq!(info["tombstoned"], false);
    assert!(
        info["size_bytes"].as_u64().unwrap() > 0,
        "size missing: {info}"
    );
    let created = info["created_at"].as_i64().unwrap();
    assert!(
        (before..=after_create + 5).contains(&created),
        "created_at {created} outside [{before},{after_create}]"
    );
    assert_eq!(
        info["tables"],
        json!([{"name": "t", "rows": 3}, {"name": "u", "rows": 0}]),
        "_dg_replay leaked or counts wrong: {info}"
    );

    // Unknown names are 404, never silently created.
    let (_, stdout, _) = fleet.cli.err("info missing", &["info", "nope"], "database_not_found");
    assert!(stdout.contains("\"status\":404"), "info missing: {stdout}");

    // Scoped credential issued while the database is live.
    let issued = fleet.cli.ok(
        "key create",
        &[
            "key",
            "create",
            "reader",
            "--role",
            "database_read_only",
            "--database",
            "info_db",
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let scoped_file = dir.path().join("reader.key");
    std::fs::write(&scoped_file, issued["key"].as_str().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&scoped_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let scoped = common::Cli {
        server: fleet.cli.server.clone(),
        key_file: scoped_file,
    };

    // After delete the name reports tombstone state instead of tables.
    assert_eq!(
        fleet.cli.ok("delete", &["delete", "info_db", "--confirm", "info_db"])["deleted"],
        true
    );
    let tomb = fleet.cli.ok("info tombstoned", &["info", "info_db"]);
    assert_eq!(tomb["tombstoned"], true);
    assert!(tomb.get("tables").is_none(), "tombstone leaked tables: {tomb}");
    assert!(
        tomb["purge_after"].as_i64().unwrap() > epoch(),
        "purge deadline missing: {tomb}"
    );

    // Scoped keys are refused on the management route.
    scoped.err("scoped info", &["info", "info_db"], "forbidden");
}
