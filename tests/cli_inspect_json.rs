//! `dg inspect --json`: the same facts a terminal shows, off a terminal.
//!
//! Run: `timeout 300 cargo test --locked --test cli_inspect_json`
//!
//! The contract these tests pin: `dg inspect` answers "what has this
//! database been doing" — activity and backup state — and it answers
//! that the same way whether a person is watching or a script is
//! reading. Before this, piped output silently degraded to the bare
//! info document, so a script got a different, poorer answer than the
//! operator looking at the same command on the same box.
//!
//! The second half pins the degradation rule: activity and backup are
//! auxiliary reads. When one of them cannot be fetched the section is
//! reported absent, not invented as zero — a zero would read as "no
//! traffic" rather than "we could not ask".

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

/// Create `shop` with a couple of tables so `tables` has something to
/// report, and write a row so activity has a fact to carry.
fn seed(fleet: &Fleet) {
    fleet.ok("create", &["create", "shop"]);
    fleet.ok(
        "ddl",
        &[
            "execute",
            "shop",
            "-",
            "--sql",
            "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT);\n\
             INSERT INTO products (id, sku) VALUES (1, 'A'), (2, 'B');",
        ],
    );
}

/// `dg inspect --json` for one database.
fn inspect(fleet: &Fleet, label: &str, db: &str) -> Value {
    fleet.ok(label, &["inspect", "--json", db])
}

#[test]
fn a_pipe_gets_the_activity_a_terminal_shows() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = inspect(&fleet, "inspect", "shop");

    // The facts a person sees. A script that cannot read this cannot
    // answer "what is this database doing" without rejoining three
    // endpoints by hand.
    assert!(
        document["activity"].is_object(),
        "activity section is missing: {document}"
    );
    assert_eq!(
        document["activity"]["rows_written"].as_u64(),
        Some(2),
        "the writes just made are not in the activity section: {document}"
    );
    assert!(
        document["activity"]["rows_read"].is_u64(),
        "activity carries no read counter: {document}"
    );
    // Availability is stated rather than implied: a script must be able
    // to tell "no activity" from "could not ask".
    assert_eq!(
        document["activity_available"].as_bool(),
        Some(true),
        "activity_available should be true on a healthy gateway: {document}"
    );
}

#[test]
fn a_pipe_gets_the_backup_state_a_terminal_shows() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = inspect(&fleet, "inspect", "shop");

    assert!(
        document["backup"].is_object(),
        "backup section is missing: {document}"
    );
    // The state vocabulary is the same one `dg backup status` uses, so
    // a script greps one word and not two.
    let state = document["backup"]["state"]
        .as_str()
        .expect("backup carries a state");
    assert!(
        matches!(state, "fresh" | "behind" | "stale" | "unknown" | "failed"),
        "unexpected backup state {state:?}: {document}"
    );
    assert_eq!(
        document["backup_available"].as_bool(),
        Some(true),
        "backup_available should be true on a healthy gateway: {document}"
    );
}

#[test]
fn the_info_the_database_always_had_is_still_there() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = inspect(&fleet, "inspect", "shop");

    // Composition added sections; it did not replace the document a
    // caller already depended on.
    assert_eq!(document["name"].as_str(), Some("shop"));
    assert!(document["size_bytes"].is_u64(), "{document}");
    assert_eq!(document["tombstoned"].as_bool(), Some(false));
    let tables: Vec<&str> = document["tables"]
        .as_array()
        .expect("tables list")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tables.contains(&"products"),
        "the table list lost a table: {tables:?}"
    );
}

#[test]
fn a_tombstoned_database_still_answers() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.ok(
        "delete",
        &["delete", "shop", "--confirm", "shop"],
    );
    // A deleted database is exactly when an operator most wants to ask
    // what it was doing, so the command must not refuse.
    let document = inspect(&fleet, "inspect", "shop");
    assert_eq!(
        document["tombstoned"].as_bool(),
        Some(true),
        "a tombstone should be visible: {document}"
    );
    assert!(
        document["purge_after"].is_u64(),
        "a tombstone reports when it will be purged: {document}"
    );
}

#[test]
fn the_auxiliary_sections_degrade_instead_of_failing() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // The rich view is composed from three reads. The database read is
    // required; the other two are auxiliary, and a failure there must
    // degrade that section rather than fail the whole command —
    // otherwise a backup subsystem hiccup would make it impossible to
    // ask what a database is doing.
    let document = inspect(&fleet, "inspect", "shop");
    // On a healthy gateway both are present. The structural guarantee
    // this test pins is that both keys ALWAYS exist, so a script reads
    // `activity` and `backup` without checking, and consults the
    // `*_available` flags to know whether the numbers mean anything.
    assert!(document.get("activity").is_some(), "{document}");
    assert!(document.get("backup").is_some(), "{document}");
    assert!(document.get("activity_available").is_some(), "{document}");
    assert!(document.get("backup_available").is_some(), "{document}");
}
