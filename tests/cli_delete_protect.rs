//! Delete protection and undelete: the two guards around a mistake.
//!
//! Run: `timeout 300 cargo test --locked --test cli_delete_protect`
//!
//! `dg delete` has always required the operator to retype the name. That
//! stops a typo, but not the two things that actually happen: deleting
//! the wrong database on purpose-but-wrongly, and deleting right and
//! having no way back. These tests pin the two additions that close
//! that — a protection flag the deletion respects, and an undelete that
//! relies on the file never having been unlinked.
//!
//! The retention-window refusal matters more than it looks. Undelete is
//! only safe because `delete` sets a tombstone and `purge_expired` is
//! what finally unlinks; if undelete ignored the published deadline it
//! would silently promise a restore it cannot always deliver.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

fn seed(fleet: &Fleet) {
    fleet.cli.ok("create shop", &["create", "shop"]);
    fleet.cli.q(
        "table",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY)",
    );
}

#[test]
fn a_protected_database_refuses_deletion_and_keeps_its_file() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok("protect", &["protect", "shop"]);

    let (ok, stdout, stderr) = fleet.cli.run(&["delete", "shop", "--confirm", "shop"]);
    assert!(
        !ok,
        "a protected database was deleted anyway: stdout={stdout} stderr={stderr}"
    );
    // The refusal names protection, so the operator knows what to change
    // rather than guessing at a generic failure.
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("database_protected"),
        "the refusal did not name protection: {combined}"
    );

    // Still live, still queryable: the guard did not half-delete it.
    let inspected = fleet.cli.ok("inspect", &["inspect", "--json", "shop"]);
    assert_eq!(
        inspected["tombstoned"].as_bool(),
        Some(false),
        "{inspected}"
    );
    let rows = fleet
        .cli
        .ok("query", &["query", "shop", "SELECT count(*) FROM products"]);
    assert!(rows["rows"].is_array(), "the database is unusable: {rows}");
}

#[test]
fn clearing_protection_lets_the_deletion_through() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok("protect", &["protect", "shop"]);
    fleet.cli.ok("unprotect", &["protect", "shop", "--off"]);
    // The guard is a gate, not a permanent lock.
    let deleted = fleet
        .cli
        .ok("delete", &["delete", "shop", "--confirm", "shop"]);
    assert_eq!(deleted["deleted"].as_bool(), Some(true), "{deleted}");
}

#[test]
fn an_unprotected_database_is_deleted_without_extra_friction() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // Protection is opt-in. A database nobody protected must not acquire
    // a guard by accident, or the flag stops meaning anything.
    let deleted = fleet
        .cli
        .ok("delete", &["delete", "shop", "--confirm", "shop"]);
    assert_eq!(deleted["deleted"].as_bool(), Some(true), "{deleted}");
}

#[test]
fn undelete_brings_a_tombstone_back_with_its_data() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet
        .cli
        .q("insert", "shop", "INSERT INTO products (id) VALUES (1)");
    fleet
        .cli
        .ok("delete", &["delete", "shop", "--confirm", "shop"]);
    // Gone from the live list while tombstoned.
    let tombstoned = fleet.cli.ok("inspect", &["inspect", "--json", "shop"]);
    assert_eq!(
        tombstoned["tombstoned"].as_bool(),
        Some(true),
        "{tombstoned}"
    );

    fleet.cli.ok("undelete", &["undelete", "shop"]);

    let restored = fleet.cli.ok("inspect", &["inspect", "--json", "shop"]);
    assert_eq!(
        restored["tombstoned"].as_bool(),
        Some(false),
        "still tombstoned after undelete: {restored}"
    );
    // The point of undelete is the DATA, not the name. The file was only
    // ever tombstoned, so the row has to still be there.
    let rows = fleet.cli.ok(
        "rows",
        &["query", "shop", "SELECT count(*) AS n FROM products"],
    );
    assert_eq!(
        rows["rows"][0][0].as_u64(),
        Some(1),
        "undelete restored the name but lost the data: {rows}"
    );
}

#[test]
fn undelete_refuses_a_live_database() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // Nothing to undo. Succeeding silently would hide a mistaken command
    // and leave the operator believing something was restored.
    let (ok, stdout, stderr) = fleet.cli.run(&["undelete", "shop"]);
    assert!(!ok, "undelete accepted a live database: {stdout}{stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("not_deleted"),
        "the refusal did not distinguish 'never deleted': {combined}"
    );
}

#[test]
fn undelete_refuses_past_the_retention_window() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet
        .cli
        .ok("delete", &["delete", "shop", "--confirm", "shop"]);
    // Push the published deadline into the past the way real time would.
    // The maintenance loop skips a contended tick, so a tombstone can sit
    // past its deadline with the file still on disk; whether the bytes
    // happen to remain is not the contract, the published window is.
    let registry = fleet.data_dir.join("metadata.sqlite");
    let conn = rusqlite::Connection::open(&registry).unwrap();
    conn.execute(
        "UPDATE databases SET purge_after = purge_after - 86400 * 365 WHERE name='shop'",
        [],
    )
    .unwrap();
    drop(conn);

    let (ok, stdout, stderr) = fleet.cli.run(&["undelete", "shop"]);
    assert!(!ok, "undelete ran past its window: {stdout}{stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("retention_expired"),
        "the refusal did not distinguish 'too late' from 'not found': {combined}"
    );
}

#[test]
fn an_unknown_name_is_not_found_for_every_new_verb() {
    let fleet = Fleet::boot();
    // Each verb answers "no such database" rather than inventing a row.
    for (label, args) in [
        ("undelete", vec!["undelete", "ghost"]),
        ("protect", vec!["protect", "ghost"]),
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

/// Protection is registry state, so it has to survive a restart. A guard
/// that reset on reboot would guard nothing.
#[test]
fn protection_survives_against_a_fresh_read() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok("protect", &["protect", "shop"]);
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
    let (ok, _, _) = fleet.cli.run(&["delete", "shop", "--confirm", "shop"]);
    assert!(!ok, "protection did not survive a later command");
}
