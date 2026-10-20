//! Contract tests for the JSON shape the scripts depend on.
//!
//! Four scripts parse the CLI's `--json` output. They are the
//! machine surface of the CLI, and a change to it breaks a deployment
//! gate rather than a test. These tests hold the shape the scripts
//! read, so a change to it has to be a deliberate one.
//!
//! The contract is additive: a field the scripts read keeps its name
//! and its meaning, and new fields may be added beside it.
//!
//! Run: `timeout 300 cargo test --locked --test cli_json_contract`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;

#[test]
fn the_query_shape_the_scripts_read_is_unchanged() {
    // scripts/sandbox-packet.sh and scripts/sandbox-s3-gates.sh read
    // `rows.0.0` -- the first column of the first row.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shape_db"]);
    fleet
        .cli
        .q("seed", "shape_db", "CREATE TABLE t (id INTEGER)");

    let result = fleet.cli.ok("query", &["query", "shape_db", "SELECT count(*) AS n FROM t"]);
    let rows = result["rows"].as_array().expect("rows is an array");
    assert_eq!(rows.len(), 1, "one row: {result}");
    assert_eq!(rows[0][0], 1, "the first column of the first row: {result}");
    assert!(
        result["columns"].is_array(),
        "the columns are named: {result}"
    );
}

#[test]
fn the_list_shape_the_scripts_read_is_unchanged() {
    // scripts/sandbox-s3-gates.sh and scripts/clean-host-phases.sh read
    // `databases` -- a list of NAMES, as strings.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "listed_db"]);

    let result = fleet.cli.ok("list", &["list"]);
    let databases = result["databases"].as_array().expect("databases is an array");
    assert!(
        databases.iter().all(|name| name.is_string()),
        "the database names are strings: {result}"
    );
    assert!(
        databases.contains(&serde_json::json!("listed_db")),
        "the database is listed: {result}"
    );
}

#[test]
fn the_status_shape_the_scripts_read_is_unchanged() {
    // scripts/sandbox-packet.sh reads the failure counters under
    // `requests`.
    let fleet = Fleet::boot();
    let result = fleet.cli.ok("status", &["status"]);
    for counter in [
        "total",
        "capacity_exceeded",
        "query_deadline_exceeded",
        "internal_error",
    ] {
        assert!(
            result["requests"][counter].is_number(),
            "`requests.{counter}` is a number: {result}"
        );
    }
    // The richer facts the new views use are ADDED, not substituted:
    // the old fields keep their names and their meaning.
    assert!(result["uptime_secs"].is_number(), "{result}");
    assert!(result["version"].is_string(), "{result}");
    assert!(
        result["database_counts"]["live"].is_number(),
        "the live count is there: {result}"
    );
}

#[test]
fn the_create_and_import_shapes_the_scripts_read_are_unchanged() {
    // scripts/clean-host-phases.sh runs `dg --json create` and
    // `dg --json import` and reads the answer.
    let fleet = Fleet::boot();
    let created = fleet.cli.ok("create", &["create", "made_db"]);
    assert_eq!(created["name"], "made_db", "{created}");

    let imported = fleet.cli.ok("import", &["import", "imported_db", "--from", "/dev/null"]);
    // The import of an empty file is refused, but the answer is still
    // the documented shape: a name, or an error envelope.
    assert!(
        imported["imported"].is_string() || imported["error"].is_object(),
        "the import answers with a name or an error: {imported}"
    );
}
