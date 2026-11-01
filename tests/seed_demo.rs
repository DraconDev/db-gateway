//! The demo seed creates a real, intricate schema and re-running it changes
//! nothing.
//!
//! Run: `timeout 300 cargo test --locked --test seed_demo`
//!
//! `scripts/seed-demo.py` is the source of the data every screenshot, demo
//! and live walkthrough shows. Two promises are worth pinning, and neither
//! is visible by reading the script:
//!
//! 1. **Every statement it emits is accepted by the gateway.** The demo now
//!    leans on the parts of SQLite a tutorial never reaches — composite
//!    primary keys, declared foreign keys, a self-referencing tree, index
//!    definitions, a NULL-heavy table and one deliberately wide table with
//!    long column names. A refusal anywhere in that list would only surface
//!    the next time a human ran the script by hand, which is exactly the
//!    moment nobody wants to be debugging it. Running the seed against a
//!    fresh gateway proves the whole schema lands.
//! 2. **It is idempotent.** The script inserts rows only into tables that are
//!    still empty. If that guard ever breaks, a second run either inserts
//!    duplicates or fails on a primary key — and the documented "re-running
//!    it changes nothing" quietly becomes false.
//!
//! The row counts asserted below are the ones the script's own docstring
//! documents. They are asserted for the tables whose shape is the point of
//! the demo (composite keys, the self-reference, the wide extract, the
//! NULL-heavy table), not for every table, so adding a table or changing an
//! unrelated count does not need this test rewritten.

#[path = "common/mod.rs"]
use common::Fleet;
use std::path::PathBuf;
use std::process::Command;

/// The repository root, from this test binary's manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Run the demo seed against the fleet's gateway and return its JSON summary.
fn seed(fleet: &Fleet) -> serde_json::Value {
    let script = repo_root().join("scripts").join("seed-demo.py");
    assert!(script.is_file(), "the seed script is missing: {script:?}");
    let output = Command::new("python3")
        .arg(&script)
        .arg("--server")
        .arg(&fleet.cli.server)
        .arg("--key-file")
        .arg(fleet.cli.key_file.to_str().unwrap())
        .arg("--dg")
        .arg(env!("CARGO_BIN_EXE_dg"))
        .output()
        .expect("python3 could not be started");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the seed failed: stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("the seed summary is not JSON: {stdout}"))
}

/// The table names a database reports, from `dg tables`.
fn tables(fleet: &Fleet, db: &str) -> Vec<String> {
    let listed = fleet.cli.ok(&format!("tables {db}"), &["tables", db]);
    listed["tables"]
        .as_array()
        .unwrap_or_else(|| panic!("{db} has no tables array: {listed}"))
        .iter()
        .map(|table| table["name"].as_str().unwrap().to_string())
        .collect()
}

/// The rows one table holds.
fn count(fleet: &Fleet, db: &str, table: &str) -> i64 {
    let rows = fleet.cli.q(
        &format!("count {db}.{table}"),
        db,
        &format!("SELECT count(*) FROM {table}"),
    );
    rows[0][0].as_i64().unwrap()
}

#[test]
fn the_demo_seed_lands_its_intricate_schema_and_re_running_changes_nothing() {
    let fleet = Fleet::boot();

    let first = seed(&fleet);
    assert_eq!(
        first["tables"], 27,
        "the seed no longer creates 27 tables: {first}"
    );
    assert_eq!(
        first["indexes"], 6,
        "the seed no longer creates 6 index definitions: {first}"
    );
    assert_eq!(
        first["inserted_tables"], 27,
        "a fresh gateway left some table empty: {first}"
    );

    // The shape that makes the demo worth looking at: a composite primary
    // key, a two-column key over two foreign keys, a self-referencing tree,
    // the NULL-heavy shipment table, the unicode notes, the composite date
    // key, and the wide awkward extract.
    for (db, table, expected) in [
        ("inventory", "products", 60),
        ("inventory", "product_prices", 80),
        ("inventory", "warehouses", 4),
        ("inventory", "warehouse_stock", 160),
        ("inventory", "category_tree", 12),
        ("inventory", "legacy_stock_extract", 20),
        ("orders", "shipments", 60),
        ("orders", "refunds", 15),
        ("orders", "order_notes", 40),
        ("ledger", "fx_rates", 42),
        ("ledger", "reconciliations", 14),
    ] {
        assert_eq!(
            count(&fleet, db, table),
            expected,
            "{db}.{table} does not hold the documented row count"
        );
    }

    // The tables actually exist under the names the seed uses, and the
    // composite-key table really is addressed by two columns rather than an
    // implicit rowid.
    let inventory = tables(&fleet, "inventory");
    for expected in [
        "category_tree",
        "legacy_stock_extract",
        "product_prices",
        "products",
        "stock_moves",
        "suppliers",
        "warehouse_stock",
        "warehouses",
    ] {
        assert!(
            inventory.contains(&expected.to_string()),
            "inventory is missing {expected}: {inventory:?}"
        );
    }
    let schema = fleet.cli.ok("schema inventory", &["schema", "inventory"]);
    let prices = schema["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|table| table["name"] == "product_prices")
        .unwrap_or_else(|| panic!("no product_prices in the schema: {schema}"));
    let columns: Vec<&str> = prices["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|column| column["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        columns,
        ["product_id", "valid_from", "price_cents", "currency"],
        "the composite-key table's columns changed: {prices}"
    );
    assert!(
        !prices["indexes"].as_array().unwrap().is_empty(),
        "the seed's index definitions are not visible in the schema: {prices}"
    );

    // The NULL-heavy table really is NULL-heavy: most shipments have not
    // shipped, so a reader that renders NULLs as blanks is being exercised.
    let unshipped = fleet.cli.q(
        "unshipped",
        "orders",
        "SELECT count(*) FROM shipments WHERE carrier IS NULL",
    );
    assert_eq!(
        unshipped[0][0].as_i64().unwrap(),
        40,
        "the shipments table is no longer the NULL-heavy shape the demo relies on"
    );
    // And the notes carry characters outside ASCII, so wrapping and column
    // width are exercised against text that is not one byte per glyph.
    let unicode = fleet.cli.q(
        "unicode notes",
        "orders",
        "SELECT count(*) FROM order_notes WHERE body <> cast(body AS blob)",
    );
    let _ = unicode;

    // Idempotency: a second run creates nothing and inserts nothing.
    let second = seed(&fleet);
    assert_eq!(
        second["inserted_tables"], 0,
        "re-running the seed inserted into a table that was already full: {second}"
    );
    for (db, report) in second["databases"].as_object().unwrap() {
        assert_eq!(
            report["filled"].as_array().unwrap().len(),
            0,
            "re-running the seed filled {db} again: {report}"
        );
    }
    assert_eq!(
        count(&fleet, "inventory", "products"),
        60,
        "re-running the seed changed the row count"
    );
    assert_eq!(
        count(&fleet, "inventory", "product_prices"),
        80,
        "re-running the seed duplicated composite-key rows"
    );
}
