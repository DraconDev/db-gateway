//! Opening a table in the terminal view: the parse, the request,
//! and what the operator reads.
//!
//! The grid is a read of the same endpoint the CLI queries, so these
//! tests pin the SQL the view sends and the words it draws from the
//! answer — without a terminal and without a gateway.

use dg::tui::actions::{ActionKind, Invocation, TABLE_PAGE_SIZE, valid_table};
use dg::tui::{open_table_outcome, tables_outcome};
use dg::views;
use reqwest::Method;
use serde_json::json;

/// A `GET /v1/databases/demo` answer with two tables.
fn info() -> serde_json::Value {
    json!({
        "name": "demo",
        "size_bytes": 16384,
        "tombstoned": false,
        "tables": [
            {"name": "products", "rows": 8},
            {"name": "suppliers", "rows": 4}
        ]
    })
}

/// A query answer with two rows.
fn rows() -> serde_json::Value {
    json!({
        "columns": ["id", "name"],
        "rows": [[1, "canvas tote"], [2, "wool scarf"]],
        "rows_affected": 0
    })
}

#[test]
fn only_plain_names_open() {
    // The table name goes inside double quotes in the statement, so a
    // quote inside it would break out. The view refuses those names
    // before anything is sent.
    assert!(valid_table("products"));
    assert!(valid_table("_litestream_seq"));
    assert!(valid_table("order_items2"));
    assert!(!valid_table(""));
    assert!(!valid_table("2fast"));
    assert!(!valid_table("my table"));
    assert!(!valid_table("a\" OR \"1\"=\"1"));
    assert!(!valid_table("drop;--"));
}

#[test]
fn open_takes_a_database_a_table_and_an_optional_page() {
    let invocation = ActionKind::OpenTable
        .parse("demo products")
        .expect("the arguments parse");
    assert_eq!(
        invocation,
        Invocation::OpenTable {
            name: "demo".to_string(),
            table: "products".to_string(),
            page: 0,
        }
    );
    let invocation = ActionKind::OpenTable
        .parse("demo products 2")
        .expect("the arguments parse");
    assert_eq!(
        invocation,
        Invocation::OpenTable {
            name: "demo".to_string(),
            table: "products".to_string(),
            page: 2,
        }
    );
    for arguments in ["demo", "demo \" OR \" ] = [\"x", "demo products two"] {
        assert!(
            ActionKind::OpenTable.parse(arguments).is_err(),
            "{arguments:?} parses but must not"
        );
    }
    assert!(
        ActionKind::Tables.parse("").is_err(),
        "tables without a database parses but must not"
    );
}

#[test]
fn an_open_table_reads_one_page_through_the_query_endpoint() {
    // The view cannot invent a call the CLI would not make: opening a
    // table is a SELECT any operator could type, fixed to one page.
    let request = ActionKind::OpenTable
        .parse("demo products 2")
        .expect("the arguments parse")
        .request()
        .expect("the invocation makes a request");
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.path, "/v1/databases/demo/query");
    let sql = request.body.as_ref().and_then(|body| body["sql"].as_str());
    assert_eq!(
        sql,
        Some("SELECT * FROM \"products\" ORDER BY rowid LIMIT 50 OFFSET 100"),
        "unexpected page SQL: {sql:?}"
    );
    assert_eq!(
        request.semantics,
        dg::client::Semantics::Read,
        "paging a grid is a read"
    );
    let request = ActionKind::Tables
        .parse("demo")
        .expect("the arguments parse")
        .request()
        .expect("the invocation makes a request");
    assert_eq!(request.method, Method::GET);
    assert_eq!(request.path, "/v1/databases/demo");
    assert_eq!(
        request.semantics,
        dg::client::Semantics::Read,
        "listing tables is a read"
    );
    assert_eq!(TABLE_PAGE_SIZE, 50);
}

#[test]
fn the_tables_view_counts_what_the_database_holds() {
    let table = views::database_tables(&info());
    let text = format!("{table:?}");
    assert!(
        text.contains("products") && text.contains("suppliers"),
        "the view omits a table: {text}"
    );
}

#[test]
fn the_tables_outcome_names_the_database() {
    let outcome = tables_outcome("demo", &info());
    assert_eq!(outcome.headline, "tables in demo");
    assert!(outcome.error.is_none());
    let table = outcome.table.expect("the outcome holds the grid");
    assert_eq!(table.len(), 2, "the grid holds one row per table");
}

#[test]
fn the_open_table_outcome_says_which_slice_is_on_screen() {
    let outcome = open_table_outcome("demo", "products", 0, &rows());
    assert_eq!(outcome.headline, "products rows 1–2");
    assert!(outcome.error.is_none());
    let table = outcome.table.expect("the outcome holds the grid");
    assert_eq!(table.len(), 2);

    let outcome = open_table_outcome("demo", "products", 3, &rows());
    assert_eq!(outcome.headline, "products rows 151–152");

    let empty = json!({"columns": ["id"], "rows": [], "rows_affected": 0});
    let outcome = open_table_outcome("demo", "products", 1, &empty);
    assert_eq!(outcome.headline, "products has no rows on page 1");
}

#[test]
fn a_refused_open_reads_like_any_other_refusal() {
    let refused =
        json!({"error": {"code": "database_not_found", "message": "no such database"}});
    for outcome in [
        tables_outcome("demo", &refused),
        open_table_outcome("demo", "products", 0, &refused),
    ] {
        assert_eq!(outcome.headline, "the gateway refused");
        assert!(outcome.table.is_none());
        assert!(
            outcome.error.unwrap_or_default().contains("database_not_found"),
            "the refusal loses the reason"
        );
    }
}

#[test]
fn the_open_grid_draws_on_a_test_terminal() {
    // The operator reads a grid, not JSON: draw the outcome the TUI
    // would show and read what an operator would read.
    use dg::tui::App;
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    app.outcome = Some(open_table_outcome("demo", "products", 0, &rows()));
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| dg::tui::draw::draw(frame, &mut app))
        .expect("the view draws");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..20 {
        for x in 0..60 {
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
        text.contains("products rows 1–2"),
        "the headline is not drawn"
    );
    assert!(
        text.contains("canvas tote"),
        "the grid rows are not drawn"
    );
}
