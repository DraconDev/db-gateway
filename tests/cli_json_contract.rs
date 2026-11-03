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

    let result = fleet.cli.ok(
        "query",
        &["query", "shape_db", "SELECT count(*) AS n FROM t"],
    );
    let rows = result["rows"].as_array().expect("rows is an array");
    assert_eq!(rows.len(), 1, "one row: {result}");
    // The count is zero: the table was created but nothing was put in
    // it. The point is the SHAPE, not the number.
    assert_eq!(rows[0][0], 0, "the first column of the first row: {result}");
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
    let databases = result["databases"]
        .as_array()
        .expect("databases is an array");
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
fn status_reports_query_latency_once_statements_run() {
    // The latency percentile sample: service-wide p50/p95 plus the
    // per-db p95, all absent (null) until a statement runs, then
    // numbers. Scripts read these the way they read the wait
    // percentiles.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "lat_db"]);
    let fresh = fleet.cli.ok("status", &["status"]);
    assert!(
        fresh["requests"]["query_p50_us"].is_null(),
        "no statements ran yet: {fresh}"
    );
    fleet.cli.q("seed", "lat_db", "CREATE TABLE t (id INTEGER)");
    fleet
        .cli
        .q("insert", "lat_db", "INSERT INTO t (id) VALUES (1)");
    let result = fleet.cli.ok("status", &["status"]);
    assert!(
        result["requests"]["query_p50_us"].is_number(),
        "the p50 is there: {result}"
    );
    assert!(
        result["requests"]["query_p95_us"].is_number(),
        "the p95 is there: {result}"
    );
    assert!(
        result["requests"]["query_samples"].as_u64().unwrap_or(0) >= 2,
        "both statements were sampled: {result}"
    );
    assert!(
        result["databases"]["lat_db"]["query_p95_us"].is_number(),
        "the per-database p95 is there: {result}"
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

    // A refused import still answers with the documented shape: an
    // error envelope, not a panic and not a bare string.
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.sqlite");
    std::fs::write(&empty, b"").unwrap();
    let (ok, stdout, _stderr) =
        fleet
            .cli
            .run(&["import", "imported_db", "--from", empty.to_str().unwrap()]);
    if ok {
        let imported: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert!(
            imported["imported"].is_string(),
            "a successful import names the database: {imported}"
        );
    } else {
        let refused: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert!(
            refused["error"]["code"].is_string(),
            "a refused import says why: {refused}"
        );
    }
}
