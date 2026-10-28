//! The database page: a database opened as its own screen.
//!
//! Opening a database is going somewhere, not reading a note. The page
//! shows what is in the database -- its tables down the side -- and the
//! selected table's values filling the rest, so switching tables is a
//! move inside the page rather than a return to the dashboard.
//!
//! Run: `timeout 300 cargo test --locked --test tui_page`

use dg::tui::actions::Invocation;
use dg::tui::{App, DatabasePage, Mode};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The structure answer for a database with three tables.
fn structure() -> serde_json::Value {
    json!({
        "database": "inventory",
        "tombstoned": false,
        "created_at": 1_700_000_000,
        "tables": [
            {"name": "suppliers", "rows": 20,
             "columns": [{"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}],
             "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE suppliers (id INTEGER PRIMARY KEY)"},
            {"name": "products", "rows": 60,
             "columns": [{"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}],
             "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE products (id INTEGER PRIMARY KEY)"},
            {"name": "stock_moves", "rows": 121,
             "columns": [{"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}],
             "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE stock_moves (id INTEGER PRIMARY KEY)"}
        ]
    })
}

/// A page over that structure, with the tables in name order as the
/// reader leaves them.
fn page() -> DatabasePage {
    let mut tables = dg::views::structures(&structure(), None);
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    DatabasePage {
        database: "inventory".to_string(),
        tables,
        selected: 0,
        list_scroll: 0,
    }
}

fn app() -> App {
    App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64))
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| dg::tui::draw::draw(frame, app))
        .expect("the view draws");
    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..height {
        let line: String = (0..width)
            .map(|x| {
                buffer
                    .cell((x, y))
                    .map(|cell| cell.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect();
        lines.push(line);
    }
    lines.join("\n")
}

#[test]
fn the_page_lists_every_table_and_selects_the_first() {
    let page = page();
    let names: Vec<&str> = page.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["products", "stock_moves", "suppliers"]);
    assert_eq!(
        page.current().map(|t| t.name.as_str()),
        Some("products"),
        "the first table is not selected"
    );
}

#[test]
fn moving_through_the_list_wraps_nothing_and_stops_at_its_ends() {
    let mut page = page();
    page.move_selection(1);
    assert_eq!(page.current().map(|t| t.name.as_str()), Some("stock_moves"));
    page.move_selection(1);
    assert_eq!(page.current().map(|t| t.name.as_str()), Some("suppliers"));
    // Past the end stays at the end: wrapping would move the operator to
    // a table they did not ask for.
    page.move_selection(1);
    assert_eq!(page.current().map(|t| t.name.as_str()), Some("suppliers"));
    page.move_selection(-1);
    assert_eq!(page.current().map(|t| t.name.as_str()), Some("stock_moves"));
    for _ in 0..5 {
        page.move_selection(-1);
    }
    assert_eq!(page.current().map(|t| t.name.as_str()), Some("products"));
}

#[test]
fn the_page_is_drawn_with_the_list_and_the_values() {
    let mut app = app();
    app.page = Some(page());
    // The values pane holds the selected table's rows.
    let values = json!({
        "columns": ["id", "sku"],
        "rows": [[1, "SKU-1001"], [2, "SKU-1002"]],
        "rows_affected": 0
    });
    let mut browser = dg::tui::Browser::new("inventory", "products");
    browser.total = Some(60);
    app.browser = Some(browser.clone());
    app.outcome = Some(dg::tui::browser_outcome(&browser, &values));
    let text = screen(&mut app, 130, 45);
    // The database's name heads the list, and every table is in it.
    for expected in [
        "inventory",
        "products",
        "stock_moves",
        "suppliers",
        "values",
    ] {
        assert!(
            text.contains(expected),
            "the page does not show {expected:?}:\n{text}"
        );
    }
    // The values are the selected table's own.
    assert!(text.contains("SKU-1001"), "the values are missing:\n{text}");
    assert!(
        text.contains("rows 1-10 of 60"),
        "the values do not carry their footer:\n{text}"
    );
    // The dashboard is gone: the page is the screen.
    assert!(
        !text.contains("rows read*"),
        "the dashboard is still drawn behind the page:\n{text}"
    );
}

#[test]
fn arrow_keys_move_the_selection_and_load_that_tables_rows() {
    let mut app = app();
    app.page = Some(page());
    app.on_key(key(KeyCode::Down));
    assert_eq!(
        app.page
            .as_ref()
            .and_then(|p| p.current())
            .map(|t| t.name.as_str()),
        Some("stock_moves"),
        "Down did not move the selection"
    );
    assert_eq!(
        app.take_pending(),
        Some(Invocation::RefreshRows),
        "moving the list did not queue the new table's rows"
    );
    app.on_key(key(KeyCode::Up));
    assert_eq!(
        app.page
            .as_ref()
            .and_then(|p| p.current())
            .map(|t| t.name.as_str()),
        Some("products")
    );
}

#[test]
fn vim_keys_do_not_move_the_selection() {
    let mut app = app();
    app.page = Some(page());
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('k')));
    assert_eq!(
        app.page
            .as_ref()
            .and_then(|p| p.current())
            .map(|t| t.name.as_str()),
        Some("products"),
        "the page should use intuitive arrow navigation, not Vim keys"
    );
    assert_eq!(app.take_pending(), None);
}

#[test]
fn enter_opens_the_selected_table_in_the_rows_browser() {
    let mut app = app();
    let mut page = page();
    page.move_selection(1);
    app.page = Some(page);
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.take_pending(),
        Some(Invocation::OpenTable {
            name: "inventory".to_string(),
            table: "stock_moves".to_string(),
            page: 0
        }),
        "Enter did not open the selected table"
    );
    assert!(
        app.page.is_none(),
        "the page stayed open over the rows browser"
    );
}

#[test]
fn esc_returns_to_the_dashboard() {
    let mut app = app();
    app.page = Some(page());
    app.on_key(key(KeyCode::Esc));
    assert!(app.page.is_none(), "Esc did not leave the page");
    assert!(!app.quit, "Esc quit the view instead of leaving the page");
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn an_empty_database_says_so_instead_of_drawing_an_empty_page() {
    // A database with no tables is not a page with nothing on it: the
    // operator needs to be told there is nothing in it.
    let mut app = app();
    app.page = None;
    app.outcome = Some(dg::tui::Outcome {
        headline: "shop has no tables".to_string(),
        facts: vec![("database".to_string(), "shop".to_string())],
        table: None,
        error: None,
    });
    let text = screen(&mut app, 120, 40);
    assert!(text.contains("has no tables"), "{text}");
}

/// A click on a table name in the list selects that table.
///
/// Clicking a table in the list selects it directly, and the values on
/// the right follow.
#[test]
fn a_click_on_a_table_name_selects_it() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = app();
    app.page = Some(page());
    // Draw first: the click is resolved against the drawn layout.
    let text = screen(&mut app, 130, 45);
    let (column, row) = {
        let mut found = None;
        for (y, line) in text.lines().enumerate() {
            if let Some(byte) = line.find("suppliers") {
                use unicode_width::UnicodeWidthStr;
                found = Some((line[..byte].width() as u16, y as u16));
                break;
            }
        }
        found.expect("the suppliers row was not drawn")
    };
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.page
            .as_ref()
            .and_then(|p| p.current())
            .map(|t| t.name.as_str()),
        Some("suppliers"),
        "the click did not select the clicked table"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}
