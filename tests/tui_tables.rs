//! Opening a table in the terminal view: the parse, the request,
//! and what the operator reads.
//!
//! The grid is a read of the same endpoint the CLI queries, so these
//! tests pin the SQL the view sends and the words it draws from the
//! answer — without a terminal and without a gateway.

use dg::tui::actions::{ActionKind, Invocation, TABLE_PAGE_SIZE, valid_table};
use dg::tui::{browser_outcome, tables_outcome};
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
    assert!(valid_table("_replication_seq"));
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
    // Opening a table opens the ROWS BROWSER, which makes its own
    // reads -- the structure, the count under the filter and the page --
    // and keeps its state between them. It makes no request of its own,
    // so there is none to describe here: the one-shot grid path it used
    // to take is gone, and a second implementation of the same view
    // could drift from the browser without any test noticing.
    let error = ActionKind::OpenTable
        .parse("demo products 2")
        .expect("the arguments parse")
        .request()
        .expect_err("the open action makes no request of its own");
    assert!(
        error.message.contains("rows browser"),
        "the refusal does not say why: {}",
        error.message
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
    assert_eq!(TABLE_PAGE_SIZE, 10);
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
fn a_refused_open_reads_like_any_other_refusal() {
    let refused = json!({"error": {"code": "database_not_found", "message": "no such database"}});
    let outcome = tables_outcome("demo", &refused);
    assert_eq!(outcome.headline, "the gateway refused");
    assert!(outcome.table.is_none());
    assert!(
        outcome
            .error
            .unwrap_or_default()
            .contains("database_not_found"),
        "the refusal loses the reason"
    );
}

#[test]
fn the_open_grid_draws_on_a_test_terminal() {
    // The operator reads a grid, not JSON: draw the outcome the TUI
    // would show and read what an operator would read.
    use dg::tui::App;
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    let mut browser = dg::tui::Browser::new("demo", "products");
    browser.total = Some(2);
    app.outcome = Some(browser_outcome(&browser, &rows()));
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
        text.contains("products in demo"),
        "the headline is not drawn:\n{text}"
    );
    assert!(text.contains("canvas tote"), "the grid rows are not drawn");
}

/// A key press.
fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
}

#[test]
fn enter_on_the_databases_tab_opens_the_scrolled_to_database() {
    // The keyboard's doorway to a database is Enter: the database
    // under the scroll opens as its own page, exactly as a click
    // on its row does. The open-table action stays in the palette
    // for the operator who types `:` — Enter itself opens.
    use dg::tui::actions::Invocation;
    use dg::tui::{App, Mode};

    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    app.list = Some(json!({"databases": ["orders", "inventory"]}));
    // Sorted, the first row is inventory; scroll to the second row.
    app.scroll.databases = 1;
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    assert_eq!(app.mode, Mode::Normal, "Enter left the dashboard");
    assert_eq!(
        app.take_pending(),
        Some(Invocation::OpenDatabase {
            name: "orders".to_string()
        }),
        "Enter did not open the scrolled-to database"
    );
}

#[test]
fn enter_on_the_locations_tab_opens_the_scrolled_to_database() {
    // The locations tab names databases too, and its rows open the
    // same way: Enter reads the name off the scrolled-to row of the
    // table the tab drew, so the doorway and the tab cannot disagree
    // about which database is which.
    use dg::tui::actions::Invocation;
    use dg::tui::{App, Focus, Mode};

    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    app.status = Some(json!({"deployment": {"databases": {
        "orders": {"exists": true, "tombstoned": false,
                   "file": "/var/lib/dg/orders.sqlite", "size_bytes": 16384},
        "inventory": {"exists": true, "tombstoned": false,
                      "file": "/var/lib/dg/inventory.sqlite", "size_bytes": 16384}
    }}}));
    app.focus = Focus::Locations;
    // The rows sort by name, so the second row is orders.
    app.scroll.locations = 1;
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    assert_eq!(app.mode, Mode::Normal, "Enter left the dashboard");
    assert_eq!(
        app.take_pending(),
        Some(Invocation::OpenDatabase {
            name: "orders".to_string()
        }),
        "Enter did not open the scrolled-to database"
    );
}

#[test]
fn enter_on_a_tab_without_databases_opens_nothing() {
    // Only the tabs whose rows name databases open one: everywhere
    // else Enter is not a doorway and queues nothing.
    use dg::tui::{App, Focus, Mode};

    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    app.list = Some(json!({"databases": ["orders", "inventory"]}));
    app.focus = Focus::Keys;
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    assert_eq!(app.mode, Mode::Normal, "Enter left the dashboard");
    assert_eq!(app.take_pending(), None, "Enter opened something");
}

#[test]
fn a_full_page_of_rows_is_all_drawn() {
    // The grid fetches one page per request, and the last-action
    // pane draws every row of that page: a page whose rows did not
    // fit the pane used to silently drop them (fifty fetched, ten
    // drawn). The row count comes from the page SQL itself, so the
    // test fails if the fetch ever outgrows what the pane draws:
    // build a full page, then read every one of its rows back
    // from a drawn terminal.
    use dg::tui::App;
    use dg::tui::actions::TABLE_PAGE_SIZE;
    use ratatui::{Terminal, backend::TestBackend};

    let count = TABLE_PAGE_SIZE as usize;
    assert!(count > 0, "a page holds no rows");
    let response = json!({
        "columns": ["id", "name"],
        "rows": (1..=count)
            .map(|i| json!([i, format!("item {i}")]))
            .collect::<Vec<_>>(),
        "rows_affected": 0,
    });
    let mut browser = dg::tui::Browser::new("demo", "products");
    browser.total = Some(count as u64);
    let outcome = browser_outcome(&browser, &response);
    assert_eq!(
        outcome
            .table
            .as_ref()
            .expect("the outcome holds the grid")
            .len(),
        count,
        "the grid dropped rows of its own page"
    );
    let mut app = App::new("http://127.0.0.1:1".to_string(), "0".repeat(64));
    app.outcome = Some(outcome);
    // The grid the browser draws carries a footer line naming the
    // slice, so the pane needs a row more than the grid alone.
    let backend = TestBackend::new(60, 40);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| dg::tui::draw::draw(frame, &mut app))
        .expect("the view draws");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..40 {
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
    for index in [1, count / 2, count] {
        assert!(
            text.contains(&format!("item {index}")),
            "row {index} of the page is not drawn"
        );
    }
}

#[test]
fn an_overflowing_answer_still_reaches_the_grid() {
    // Twelve rows are more than one page: the outcome still holds
    // every row it was given, with the headline naming the whole
    // slice, so a bigger answer is never silently cut to the page
    // before the grid sees it. Paging itself stays at the page
    // size; this pins the builder, the page test pins the drawing.
    let response = json!({
        "columns": ["id", "name"],
        "rows": (1..=12)
            .map(|i| json!([i, format!("item {i}")]))
            .collect::<Vec<_>>(),
        "rows_affected": 0,
    });
    let mut browser = dg::tui::Browser::new("demo", "products");
    browser.total = Some(12);
    let outcome = browser_outcome(&browser, &response);
    let grid = outcome.table.expect("the outcome holds the grid");
    assert_eq!(grid.len(), 12, "the outcome cut the answer");
}
