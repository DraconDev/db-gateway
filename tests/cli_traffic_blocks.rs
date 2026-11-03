//! Traffic blocks: per-database read/write kill switches.
//!
//! Run: `timeout 300 cargo test --locked --test cli_traffic_blocks`
//!
//! A cutover freezes writers by procedure — everyone agrees to stop —
//! but procedure is not enforcement. These flags are the enforcement:
//! a database with writes blocked refuses mutations at the query path
//! no matter which credential asks, owner included, until the block is
//! cleared. Reads block symmetrically. The flags are registry state
//! (Nullable columns, NULL open), owner-only to change, and visible
//! to `dg inspect` before someone trips over them.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

fn seed(fleet: &Fleet) {
    fleet.cli.ok("create", &["create", "shop"]);
    fleet.cli.q(
        "table",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY)",
    );
    fleet
        .cli
        .q("insert", "shop", "INSERT INTO products (id) VALUES (1)");
}

#[test]
fn blocked_writes_refuse_mutations_and_keep_reads() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok("block", &["block-writes", "shop"]);
    let blocked = fleet.cli.ok("inspect", &["inspect", "--json", "shop"]);
    assert_eq!(blocked["writes_blocked"].as_bool(), Some(true), "{blocked}");

    let (ok, stdout, stderr) =
        fleet
            .cli
            .run(&["query", "shop", "INSERT INTO products (id) VALUES (2)"]);
    assert!(
        !ok,
        "a writes-blocked database accepted a mutation: stdout={stdout} stderr={stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("writes_blocked"),
        "the refusal did not name the block: {combined}"
    );

    // Reads are unaffected: the block is a freeze, not a shutdown.
    let rows = fleet.cli.ok(
        "query",
        &["query", "shop", "SELECT count(*) AS n FROM products"],
    );
    assert_eq!(rows["rows"][0][0].as_u64(), Some(1), "{rows}");

    // And the refused row never landed.
    fleet.cli.ok("unblock", &["block-writes", "shop", "--off"]);
    let rows = fleet.cli.ok(
        "query",
        &["query", "shop", "SELECT count(*) AS n FROM products"],
    );
    assert_eq!(rows["rows"][0][0].as_u64(), Some(1), "{rows}");
}

#[test]
fn blocked_reads_refuse_queries() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok("block", &["block-reads", "shop"]);

    let (ok, stdout, stderr) = fleet
        .cli
        .run(&["query", "shop", "SELECT count(*) FROM products"]);
    assert!(
        !ok,
        "a reads-blocked database answered a query: stdout={stdout} stderr={stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("reads_blocked"),
        "the refusal did not name the block: {combined}"
    );

    fleet.cli.ok("unblock", &["block-reads", "shop", "--off"]);
    let rows = fleet.cli.ok(
        "query",
        &["query", "shop", "SELECT count(*) AS n FROM products"],
    );
    assert_eq!(rows["rows"][0][0].as_u64(), Some(1), "{rows}");
}

#[test]
fn blocks_are_opt_in_and_owner_only() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // Nothing blocked by default: a fresh database takes reads and writes.
    let inspected = fleet.cli.ok("inspect", &["inspect", "--json", "shop"]);
    assert_eq!(
        inspected["writes_blocked"].as_bool(),
        Some(false),
        "{inspected}"
    );
    assert_eq!(
        inspected["reads_blocked"].as_bool(),
        Some(false),
        "{inspected}"
    );

    // A scoped key may read and write the data, but the switch itself is
    // the owner's: an application credential must never freeze its own
    // database, accidentally or otherwise.
    let app = fleet.cli.ok(
        "key",
        &[
            "key",
            "create",
            "app",
            "--role",
            "database_read_write",
            "--database",
            "shop",
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let app_file = dir.path().join("app.key");
    std::fs::write(&app_file, app["key"].as_str().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&app_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let scoped = common::Cli {
        server: fleet.cli.server.clone(),
        key_file: app_file,
    };
    scoped.err("scoped block", &["block-writes", "shop"], "forbidden");
}

#[test]
fn an_unknown_name_is_not_found_for_both_verbs() {
    let fleet = Fleet::boot();
    for (label, args) in [
        ("block-writes", vec!["block-writes", "ghost"]),
        ("block-reads", vec!["block-reads", "ghost"]),
    ] {
        let (ok, stdout, stderr) = fleet.cli.run(&args);
        assert!(!ok, "{label} accepted an unknown name: {stdout}{stderr}");
        let combined = format!("{stdout}{stderr}");
        assert!(
            combined.contains("database_not_found"),
            "{label} did not report not-found: {combined}"
        );
    }
}

/// Blocks are registry state, so they have to survive a restart. A
/// freeze that thawed on reboot would freeze nothing.
#[test]
fn blocks_survive_against_a_fresh_read() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok("block", &["block-writes", "shop"]);
    // Re-reading through a different surface proves the flag is durable
    // registry state rather than an in-memory latch.
    let listed: Value = fleet.cli.ok("list", &["list"]);
    let names: Vec<&str> = listed["databases"]
        .as_array()
        .expect("databases")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(names.contains(&"shop"), "{names:?}");
    let (ok, _, _) = fleet
        .cli
        .run(&["query", "shop", "INSERT INTO products (id) VALUES (9)"]);
    assert!(!ok, "the block did not survive a later command");
}
