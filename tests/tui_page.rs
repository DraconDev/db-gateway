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

/// A database with enough tables to extend beyond a short screen.
fn tall_page(count: usize) -> DatabasePage {
    let table = page().tables[0].clone();
    let mut tables: Vec<_> = (0..count)
        .map(|index| {
            let mut table = table.clone();
            table.name = format!("table_{index:02}");
            table
        })
        .collect();
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
/// The row count must be visible in the table list.
///
/// `docs/tui.md` draws the page as `│▸ products  60│` -- the count sits
/// inside the list pane. The line is built as marker + name + a
/// five-cell right-aligned count, but the pane's inner area is two cells
/// narrower than the width the line was sized against, so the count was
/// always clipped off the right edge and the operator never saw it. The
/// name also lost the last cell, so a cut name lost the '…' that says
/// it was cut.
#[test]
fn the_table_list_shows_the_row_count() {
    let mut app = app();
    app.page = Some(page());
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

    // The selected table's row count (60) is drawn in the list pane.
    let list_line = text
        .lines()
        .find(|line| line.contains("products"))
        .unwrap_or_else(|| panic!("the products row is not drawn:\n{text}"));
    assert!(
        list_line.contains("60"),
        "the row count is not visible in the table list: {list_line:?}\n{text}"
    );
}

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
        "click select",
        "enter open rows",
        "esc back",
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
fn leaving_a_tab_clears_a_one_time_credential_outcome() {
    let mut app = app();
    app.focus = dg::tui::Focus::Keys;
    app.outcome = Some(dg::tui::Outcome {
        headline: "issued robot".to_string(),
        facts: vec![("key".to_string(), "one-time-secret".to_string())],
        table: None,
        error: None,
    });

    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, dg::tui::Focus::Queries);
    assert!(
        app.outcome.is_none(),
        "a raw credential remained visible after leaving its result tab"
    );
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

#[test]
fn moving_past_the_visible_list_keeps_the_selection_on_screen() {
    let mut app = app();
    app.page = Some(tall_page(30));
    for _ in 0..15 {
        app.on_key(key(KeyCode::Down));
    }

    let text = screen(&mut app, 100, 16);
    let selected_name = app
        .page
        .as_ref()
        .and_then(DatabasePage::current)
        .map(|table| table.name.as_str());
    assert_eq!(selected_name, Some("table_15"));
    assert!(
        text.lines().any(|line| line.contains("▸ table_15")),
        "the selected table must stay visible in the scrolling list; scroll={}\n{text}",
        app.page.as_ref().map_or(0, |page| page.list_scroll)
    );
    assert!(
        app.page.as_ref().is_some_and(|page| page.list_scroll > 0),
        "the list viewport did not follow the selected table"
    );

    // Growing the terminal makes every table fit again; the list should
    // release its old scroll rather than leave the first names hidden.
    let expanded = screen(&mut app, 100, 45);
    assert!(
        expanded.lines().any(|line| line.contains("▸ table_15")),
        "the selection disappeared after growing the terminal:\n{expanded}"
    );
    assert_eq!(
        app.page.as_ref().map(|page| page.list_scroll),
        Some(0),
        "the old list offset was not clamped after resize"
    );
}

#[test]
fn a_click_after_list_scroll_selects_the_table_at_that_visible_row() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use unicode_width::UnicodeWidthStr;

    let mut app = app();
    app.page = Some(tall_page(30));
    for _ in 0..15 {
        app.on_key(key(KeyCode::Down));
    }
    let text = screen(&mut app, 100, 16);
    let _ = app.take_pending();
    let (column, row) = text
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            let byte = line.find("table_12")?;
            Some((line[..byte].width() as u16, row as u16))
        })
        .expect("table_12 should be visible after scrolling the table list");
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.page
            .as_ref()
            .and_then(DatabasePage::current)
            .map(|table| table.name.as_str()),
        Some("table_12"),
        "clicking a scrolled list row selected the wrong table"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}

#[test]
fn the_wheel_over_the_table_list_moves_the_table_selection() {
    use ratatui::crossterm::event::{MouseEvent, MouseEventKind};
    use unicode_width::UnicodeWidthStr;

    let mut app = app();
    app.page = Some(tall_page(30));
    let mut browser = dg::tui::Browser::new("inventory", "table_00");
    browser.total = Some(30);
    app.browser = Some(browser);
    let text = screen(&mut app, 100, 16);
    let (column, row) = text
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            let byte = line.find("table_00")?;
            Some((line[..byte].width() as u16, row as u16))
        })
        .expect("the first table should be visible");

    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.page
            .as_ref()
            .and_then(DatabasePage::current)
            .map(|table| table.name.as_str()),
        Some("table_03"),
        "the wheel over the table list should move the selection by three rows"
    );
    assert_eq!(
        app.browser.as_ref().map(|browser| browser.page),
        Some(0),
        "the list wheel paged the values pane behind the cursor"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}

#[test]
fn the_wheel_over_values_pages_rows_without_changing_the_table() {
    use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

    let mut app = app();
    app.page = Some(tall_page(30));
    let mut browser = dg::tui::Browser::new("inventory", "table_00");
    browser.total = Some(30);
    app.browser = Some(browser);
    let _ = screen(&mut app, 100, 16);

    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 80,
        row: 4,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.page
            .as_ref()
            .and_then(DatabasePage::current)
            .map(|table| table.name.as_str()),
        Some("table_00"),
        "the values wheel changed the selected table"
    );
    assert_eq!(app.browser.as_ref().map(|browser| browser.page), Some(1));
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}

#[test]
fn the_table_list_wheel_stops_at_the_last_table() {
    use ratatui::crossterm::event::{MouseEvent, MouseEventKind};
    use unicode_width::UnicodeWidthStr;

    let mut app = app();
    let mut page = tall_page(30);
    page.selected = 29;
    app.page = Some(page);
    let mut browser = dg::tui::Browser::new("inventory", "table_29");
    browser.total = Some(30);
    app.browser = Some(browser);
    let text = screen(&mut app, 100, 16);
    let (column, row) = text
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            let byte = line.find("table_29")?;
            Some((line[..byte].width() as u16, row as u16))
        })
        .expect("the selected last table should be visible");

    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.page
            .as_ref()
            .and_then(DatabasePage::current)
            .map(|table| table.name.as_str()),
        Some("table_29")
    );
    assert_eq!(app.browser.as_ref().map(|browser| browser.page), Some(0));
    assert_eq!(app.take_pending(), None);
}

/// The page is the whole screen: the dashboard's tab strip is not drawn
/// on top of it.
///
/// The strip used to be drawn above the page, so the dashboard's menu
/// came along to a place the operator had gone to. That also made a tab
/// click a no-op: it moved the tab underneath a page that stayed on
/// screen, so nothing changed and Esc then landed on a tab nobody chose.
#[test]
fn the_page_takes_the_whole_screen_with_no_tab_strip() {
    let mut app = app();
    app.page = Some(page());
    let text = screen(&mut app, 100, 24);

    // The page is there: its database, its tables, its values block.
    assert!(text.contains("inventory"), "the page is missing:\n{text}");
    assert!(
        text.contains("products"),
        "the table list is missing:\n{text}"
    );
    assert!(
        text.contains("values"),
        "the values pane is missing:\n{text}"
    );

    // The dashboard's furniture is not. Every other tab is named only by
    // the strip, so if the strip is gone none of those names appear.
    for other in ["service", "backups", "keys", "queries", "locations"] {
        assert!(
            !text.contains(other),
            "the dashboard tab strip is still on the page ({other}):\n{text}"
        );
    }
    // And the bar does not advertise tab numbers the screen cannot show.
    assert!(
        !text.contains("1-6 tabs"),
        "the status bar still advertises the tab strip:\n{text}"
    );
    // The way out is on the bar, because a place with one exit has to say
    // where it is.
    assert!(
        text.contains("esc back"),
        "the page does not say how to leave it:\n{text}"
    );
}

/// Esc leaves the page and the dashboard comes back with its strip.
#[test]
fn escaping_the_page_brings_the_dashboard_strip_back() {
    let mut app = app();
    app.page = Some(page());
    assert!(
        !screen(&mut app, 100, 24).contains("locations"),
        "the strip was drawn on the page"
    );
    app.on_key(key(KeyCode::Esc));
    assert!(app.page.is_none(), "esc did not leave the page");
    let dashboard = screen(&mut app, 100, 24);
    for tab in [
        "databases",
        "service",
        "backups",
        "keys",
        "queries",
        "locations",
    ] {
        assert!(
            dashboard.contains(tab),
            "the dashboard strip is missing {tab:?} after esc:\n{dashboard}"
        );
    }
    assert!(
        dashboard.contains("1-6 tabs"),
        "the dashboard bar does not advertise its tabs again:\n{dashboard}"
    );
}

/// A tab key pressed on the page does not move the tab underneath it.
///
/// With no strip on screen, `3` must not silently switch the hidden
/// dashboard to backups: the operator would leave the page and find
/// themselves somewhere they never asked to be.
#[test]
fn tab_keys_do_not_move_the_tab_underneath_the_page() {
    let mut app = app();
    app.page = Some(page());
    app.focus = dg::tui::Focus::Databases;
    app.on_key(key(KeyCode::Char('3')));
    assert!(app.page.is_some(), "a number key closed the page");
    assert_eq!(
        app.focus,
        dg::tui::Focus::Databases,
        "a number key moved the hidden dashboard tab"
    );
    app.on_key(key(KeyCode::Tab));
    assert_eq!(
        app.focus,
        dg::tui::Focus::Databases,
        "tab moved the hidden dashboard tab"
    );
    // The page's own keys still work.
    app.on_key(key(KeyCode::Down));
    assert_eq!(
        app.page
            .as_ref()
            .and_then(DatabasePage::current)
            .map(|t| t.name.as_str()),
        Some("stock_moves"),
        "the page stopped responding to its own keys"
    );
    // And leaving returns to the tab the operator actually chose.
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.focus, dg::tui::Focus::Databases);
}

/// A table list longer than the screen says how much of it is showing.
///
/// With forty tables the page drew the first nine-and-twenty and no sign
/// that the rest existed. The rows table has always reported its slice;
/// the table list is the same question about the same kind of list.
#[test]
fn a_clipped_table_list_reports_its_slice() {
    let mut app = app();
    app.page = Some(tall_page(30));
    let text = screen(&mut app, 100, 24);
    assert!(
        text.contains("of 30"),
        "the clipped list does not say how many tables there are:\n{text}"
    );
    assert!(
        text.contains("1-"),
        "the clipped list does not say which slice is showing:\n{text}"
    );
}

/// A list that fits says nothing about slices, because there are none.
#[test]
fn a_table_list_that_fits_does_not_report_a_slice() {
    let mut app = app();
    app.page = Some(page());
    let text = screen(&mut app, 100, 24);
    assert!(
        !text.contains(" of 3 "),
        "a complete list reported a slice it does not have:\n{text}"
    );
}
