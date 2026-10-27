//! `dg schema`: the structure of a database's tables.
//!
//! Run: `timeout 300 cargo test --locked --test cli_schema`
//!
//! The question this answers is "what is in this table": the columns,
//! their declared types, whether they may be null, whether they are the
//! primary key, and the table's indexes and foreign keys. It is read
//! from the database file with SQLite's own PRAGMAs, because the query
//! endpoint refuses PRAGMA.
//!
//! The contract these tests pin: the route answers the structure of
//! the database, the pipe prints the gateway's own document, and the
//! terminal draws it.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

/// A database with one table that has every kind of column fact.
fn seed(fleet: &Fleet) {
    fleet.cli.ok("create", &["create", "shop"]);
    fleet.cli.q(
        "create products",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT NOT NULL, \
         price_cents INTEGER DEFAULT 0, category TEXT UNIQUE)",
    );
    fleet.cli.q(
        "create orders",
        "shop",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, product_id INTEGER \
         REFERENCES products(id) ON DELETE CASCADE, note TEXT)",
    );
    fleet.cli.q(
        "insert",
        "shop",
        "INSERT INTO products VALUES (1, 'SKU-1', 100, 'bags')",
    );
}

/// The structure of one table, as the gateway's document.
fn structure(fleet: &Fleet, table: &str) -> Value {
    let document = fleet.cli.ok("schema", &["schema", "shop", table]);
    let tables = document["tables"]
        .as_array()
        .expect("the document has tables");
    assert_eq!(
        tables.len(),
        1,
        "one table was asked for, more came back: {document}"
    );
    tables[0].clone()
}

#[test]
fn every_column_carries_its_type_and_its_flags() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let table = structure(&fleet, "products");
    assert_eq!(table["name"], "products");
    assert_eq!(table["rows"], 1);
    let columns = table["columns"].as_array().expect("the table has columns");
    let names: Vec<&str> = columns
        .iter()
        .map(|column| column["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["id", "sku", "price_cents", "category"]);
    // The declared type is the schema's own, not a guess.
    assert_eq!(columns[0]["type"], "INTEGER");
    assert_eq!(columns[1]["type"], "TEXT");
    assert_eq!(columns[2]["type"], "INTEGER");
    // The flags say what the schema says: the key, the not-null, the
    // default. A blank cell would read as "not known".
    assert_eq!(columns[0]["primary_key"], true);
    assert_eq!(columns[0]["not_null"], false);
    assert_eq!(columns[1]["not_null"], true);
    assert_eq!(columns[2]["default"], "0");
    assert_eq!(columns[3]["default"], Value::Null);
}

#[test]
fn a_table_reports_its_indexes_and_its_foreign_keys() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // The UNIQUE constraint on category gives the table an index.
    let products = structure(&fleet, "products");
    let indexes = products["indexes"]
        .as_array()
        .expect("the indexes are listed");
    assert_eq!(
        indexes.len(),
        1,
        "one unique constraint, one index: {products}"
    );
    assert_eq!(indexes[0]["unique"], true);
    assert_eq!(indexes[0]["columns"][0], "category");
    // The foreign key names its target and what a delete does.
    let orders = structure(&fleet, "orders");
    let keys = orders["foreign_keys"]
        .as_array()
        .expect("the foreign keys are listed");
    assert_eq!(keys.len(), 1, "one foreign key: {orders}");
    assert_eq!(keys[0]["from"], "product_id");
    assert_eq!(keys[0]["table"], "products");
    assert_eq!(keys[0]["to"], "id");
    assert_eq!(keys[0]["on_delete"], "CASCADE");
    // The statement that made it is kept verbatim.
    assert!(
        orders["sql"]
            .as_str()
            .unwrap_or("")
            .starts_with("CREATE TABLE orders"),
        "the create statement is not kept: {orders}"
    );
}

#[test]
fn every_table_is_listed_when_no_table_is_named() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = fleet.cli.ok("schema all", &["schema", "shop"]);
    let names: Vec<&str> = document["tables"]
        .as_array()
        .expect("the document has tables")
        .iter()
        .map(|table| table["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["orders", "products"]);
}

#[test]
fn an_unknown_table_is_not_invented() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = fleet
        .cli
        .ok("schema missing", &["schema", "shop", "nosuch"]);
    assert!(
        document["tables"].as_array().unwrap().is_empty(),
        "a table that does not exist was reported: {document}"
    );
}

#[test]
fn an_unknown_database_is_refused() {
    let fleet = Fleet::boot();
    let (success, stdout, stderr) = fleet.cli.run(&["schema", "nosuchdb"]);
    assert!(!success, "a missing database was answered: {stdout}");
    assert!(
        stdout.contains("database_not_found") || stderr.contains("database_not_found"),
        "the refusal does not name the reason: stdout={stdout} stderr={stderr}"
    );
}

/// Strip ANSI escapes so a terminal's colouring does not break a text
/// assertion.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

#[test]
fn the_structure_is_drawn_in_a_terminal() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let command = format!(
        "{} --server {} --key-file {} schema shop products",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap()
    );
    let output = std::process::Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .output()
        .unwrap();
    let screen = plain(&String::from_utf8_lossy(&output.stdout));
    // The column names and their types are on screen: that is the
    // answer the operator came for.
    for expected in ["sku", "TEXT", "INTEGER", "not null", "primary key"] {
        assert!(
            screen.contains(expected),
            "the drawing does not show {expected:?}: {screen}"
        );
    }
    assert!(
        screen.contains("default"),
        "the drawing does not show the default column: {screen}"
    );
}
