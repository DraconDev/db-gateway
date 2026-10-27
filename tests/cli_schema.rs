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
fn the_replications_own_tables_are_not_the_operators_data() {
    // A replicated database holds the replication's own bookkeeping
    // tables. The structure view describes the operator's data, so
    // those stay out of it; the file's own listing still shows them.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shop"]);
    fleet.cli.q(
        "create products",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT)",
    );
    fleet.cli.q(
        "create replication bookkeeping",
        "shop",
        "CREATE TABLE _litestream_seq (id INTEGER PRIMARY KEY, seq INTEGER)",
    );
    let document = fleet.cli.ok("schema", &["schema", "shop"]);
    let names: Vec<&str> = document["tables"]
        .as_array()
        .expect("the document has tables")
        .iter()
        .map(|table| table["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["products"]);
    // The file's own listing still tells the whole truth.
    let info = fleet.cli.ok("info", &["info", "shop"]);
    let listed: Vec<&str> = info["tables"]
        .as_array()
        .expect("the info has tables")
        .iter()
        .map(|table| table["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        listed.contains(&"_litestream_seq"),
        "the file's listing hides a table of the file: {info}"
    );
}

#[test]
fn the_route_answers_one_databases_tables_over_http() {
    // The CLI prints the gateway's JSON unchanged on a pipe, so the
    // CLI tests already read the route's document — but this pins the
    // route itself: the method, the path, and the document's shape,
    // with no CLI in between.
    let fleet = Fleet::boot();
    seed(&fleet);
    let key = std::fs::read_to_string(&fleet.cli.key_file).expect("the key file reads");
    let client = reqwest::blocking::Client::new();
    let get = |path: &str| {
        let response = client
            .get(format!("{}{path}", fleet.cli.server))
            .bearer_auth(key.trim())
            .send()
            .expect("the route answers");
        let status = response.status().as_u16();
        let body: Value = response.json().expect("the answer is JSON");
        (status, body)
    };
    let (status, document) = get("/v1/databases/shop/schema");
    assert_eq!(status, 200, "the route answers: {document}");
    assert_eq!(document["database"], "shop");
    let tables = document["tables"]
        .as_array()
        .expect("the document has tables");
    assert_eq!(tables.len(), 2, "both tables are listed: {document}");
    let products = tables
        .iter()
        .find(|table| table["name"] == "products")
        .expect("products is listed");
    let columns = products["columns"]
        .as_array()
        .expect("the table has columns");
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0]["name"], "id");
    assert_eq!(columns[0]["type"], "INTEGER");
    assert_eq!(columns[0]["not_null"], false);
    assert_eq!(columns[0]["primary_key"], true);
    assert_eq!(columns[1]["not_null"], true);
    let indexes = products["indexes"]
        .as_array()
        .expect("the table has indexes");
    assert_eq!(
        indexes.len(),
        1,
        "the unique constraint's index is listed: {products}"
    );
    assert_eq!(indexes[0]["unique"], true);
    // A table narrows the answer on the server.
    let (status, one) = get("/v1/databases/shop/schema?table=orders");
    assert_eq!(status, 200, "the narrowed route answers: {one}");
    let names: Vec<&str> = one["tables"]
        .as_array()
        .expect("the narrowed document has tables")
        .iter()
        .map(|table| table["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["orders"]);
    // An unknown database is a 404 with the stable code.
    let (status, missing) = get("/v1/databases/nosuchdb/schema");
    assert_eq!(status, 404, "a missing database is a 404: {missing}");
    assert_eq!(missing["error"]["code"], "database_not_found");
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
