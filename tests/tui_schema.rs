//! The terminal view's structure read: what one table is made of.
//!
//! The row grid says which columns a table has; the structure says
//! what each column is -- its type, whether it may be null, whether it
//! is the key -- and what indexes and foreign keys the table carries.
//! These tests pin the request the view makes and what the operator
//! reads from the answer.
//!
//! Run: `timeout 300 cargo test --locked --test tui_schema`

use dg::tui::actions::{ActionKind, Invocation};
use dg::tui::schema_outcome;
use reqwest::Method;
use serde_json::json;

/// The gateway's answer for one table with a key, a not-null column,
/// a default and a unique index.
fn structure() -> serde_json::Value {
    json!({
        "database": "shop",
        "tombstoned": false,
        "tables": [{
            "name": "products",
            "rows": 60,
            "columns": [
                {"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null},
                {"name": "sku", "type": "TEXT", "not_null": true, "primary_key": false, "default": null},
                {"name": "price_cents", "type": "INTEGER", "not_null": false, "primary_key": false, "default": "0"}
            ],
            "indexes": [
                {"name": "sqlite_autoindex_products_1", "unique": true, "columns": ["sku"]}
            ],
            "foreign_keys": [],
            "sql": "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT NOT NULL, price_cents INTEGER DEFAULT 0)"
        }]
    })
}

#[test]
fn the_view_reads_the_structure_over_the_documented_route() {
    // The view cannot invent a call the CLI would not make: the
    // structure is the gateway's own route, and a table narrows it on
    // the server so the document is the API's either way.
    let request = ActionKind::Schema
        .parse("shop products")
        .expect("the arguments parse")
        .request()
        .expect("the invocation makes a request");
    assert_eq!(request.method, Method::GET);
    assert_eq!(request.path, "/v1/databases/shop/schema?table=products");
    assert_eq!(
        request.semantics,
        dg::client::Semantics::Read,
        "reading a structure is a read"
    );

    // Without a table, the same route answers for every table.
    let request = ActionKind::Schema
        .parse("shop")
        .expect("the arguments parse")
        .request()
        .expect("the invocation makes a request");
    assert_eq!(request.path, "/v1/databases/shop/schema");

    // The invocation keeps the table it was given.
    assert_eq!(
        ActionKind::Schema.parse("shop products").unwrap(),
        Invocation::Schema {
            name: "shop".to_string(),
            table: Some("products".to_string()),
        }
    );
}

#[test]
fn the_outcome_names_the_table_and_carries_its_columns() {
    let outcome = schema_outcome("shop", Some("products"), &structure());
    assert!(outcome.error.is_none());
    assert_eq!(outcome.headline, "structure of products in shop");
    let facts: Vec<&str> = outcome.facts.iter().map(|(key, _)| key.as_str()).collect();
    assert!(facts.contains(&"table"), "the facts do not name the table");
    assert!(
        facts.contains(&"columns"),
        "the facts do not count the columns"
    );
    let table = outcome.table.expect("the outcome holds the grid");
    assert_eq!(table.len(), 3, "the grid holds one row per column");
    let text = format!("{table:?}");
    assert!(text.contains("sku"), "the grid omits a column: {text}");
    assert!(text.contains("TEXT"), "the grid omits the type: {text}");
    assert!(
        text.contains("INTEGER"),
        "the grid omits the declared type: {text}"
    );
}

#[test]
fn the_grid_is_drawn_on_a_test_terminal() {
    // The operator reads a grid, not JSON: draw the outcome the TUI
    // would show and read what an operator would read.
    use dg::tui::App;
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    app.outcome = Some(schema_outcome("shop", Some("products"), &structure()));
    let backend = TestBackend::new(80, 25);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| dg::tui::draw::draw(frame, &mut app))
        .expect("the view draws");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..25 {
        for x in 0..80 {
            text.push_str(
                &buffer
                    .cell((x, y))
                    .map(|cell| cell.symbol().to_string())
                    .unwrap_or_default(),
            );
        }
        text.push('\n');
    }
    assert!(
        text.contains("structure of products in shop"),
        "the headline is not drawn: {text}"
    );
    assert!(
        text.contains("price_cents"),
        "the columns are not drawn: {text}"
    );
    assert!(text.contains("TEXT"), "the types are not drawn: {text}");
}

#[test]
fn a_table_that_does_not_exist_is_not_invented() {
    let empty = json!({"database": "shop", "tables": []});
    let outcome = schema_outcome("shop", Some("nosuch"), &empty);
    assert!(
        outcome.table.is_none(),
        "a table that does not exist was drawn"
    );
    let error = outcome.error.expect("the outcome says why");
    assert!(
        error.contains("nosuch"),
        "the refusal does not name the table: {error}"
    );
}

#[test]
fn a_refused_structure_reads_like_any_other_refusal() {
    let refused = json!({"error": {"code": "database_not_found", "message": "no such database"}});
    let outcome = schema_outcome("shop", None, &refused);
    assert_eq!(outcome.headline, "the gateway refused");
    assert!(outcome.table.is_none());
    assert!(
        outcome
            .error
            .unwrap_or_default()
            .contains("database_not_found"),
        "the refusal does not carry the code"
    );
}
