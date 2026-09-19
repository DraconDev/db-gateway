//! `dg key list`: outstanding credential inventory (CLI only).
//!
//! Run: `timeout 300 cargo test --locked --test cli_keys`
//!
//! Exercises key listing through the real `dg` binary only: two issued
//! keys appear with exact scope and role, no hash material leaks, the
//! owner row is listed, revoke shrinks the list, and scoped keys are
//! refused on the management route. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

#[test]
fn key_list_audits_outstanding_access() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "keys_db"]);
    fleet.cli.ok("create2", &["create", "keys_db2"]);

    let writer = fleet.cli.ok(
        "key create writer",
        &[
            "key",
            "create",
            "writer",
            "--role",
            "database_read_write",
            "--database",
            "keys_db",
        ],
    );
    assert_eq!(writer["key"].as_str().unwrap().len(), 64);
    let reader = fleet.cli.ok(
        "key create reader",
        &[
            "key",
            "create",
            "reader",
            "--role",
            "database_read_only",
            "--database",
            "keys_db",
            "--database",
            "keys_db2",
        ],
    );
    assert_eq!(reader["key"].as_str().unwrap().len(), 64);

    let listed = fleet.cli.ok("key list", &["key", "list"]);
    assert!(
        !listed.to_string().contains("hash"),
        "hash material leaked: {listed}"
    );
    assert_eq!(
        listed["keys"],
        json!([
            {"name":"owner","role":"owner","databases":[]},
            {"name":"reader","role":"database_read_only","databases":["keys_db","keys_db2"]},
            {"name":"writer","role":"database_read_write","databases":["keys_db"]}
        ]),
        "inventory wrong: {listed}"
    );

    // Revoke shrinks the list to owner + writer.
    assert_eq!(
        fleet.cli.ok("revoke", &["key", "revoke", "reader"])["revoked"],
        true
    );
    let listed = fleet.cli.ok("key list again", &["key", "list"]);
    assert_eq!(
        listed["keys"],
        json!([
            {"name":"owner","role":"owner","databases":[]},
            {"name":"writer","role":"database_read_write","databases":["keys_db"]}
        ]),
        "post-revoke inventory wrong: {listed}"
    );

    // Scoped keys are refused on the management route.
    let dir = tempfile::tempdir().unwrap();
    let scoped_file = dir.path().join("writer.key");
    std::fs::write(&scoped_file, writer["key"].as_str().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&scoped_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let scoped = common::Cli {
        server: fleet.cli.server.clone(),
        key_file: scoped_file,
    };
    scoped.err("scoped list", &["key", "list"], "forbidden");
}
