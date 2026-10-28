//! The TUI's rows browser: what a table open in the view does.
//!
//! Run: `timeout 300 cargo test --locked --test tui_browser`
//!
//! The browser is a place the operator stays in: it holds the table, the
//! page, the sort and the filter, so every keystroke re-asks the same
//! question instead of starting over. These tests pin the state machine
//! and the footer, on a test terminal, without a gateway.

use dg::tui::actions::Invocation;
use dg::tui::{App, Browser, Mode, browser_outcome};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;

fn app() -> App {
    App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The gateway's answer for one page of a table's rows.
fn page() -> serde_json::Value {
    json!({
        "columns": ["id", "sku", "stock"],
        "rows": [[1, "AAA", 50], [2, "BBB", 5], [3, "CCC", 30]],
        "rows_affected": 0
    })
}

fn browser() -> Browser {
    Browser {
        total: Some(62),
        ..Browser::new("shop", "products")
    }
}

#[test]
fn a_new_browser_is_on_the_first_page_and_says_so() {
    let browser = browser();
    assert_eq!(browser.page, 0);
    assert_eq!(browser.first_row(), 1);
    assert_eq!(browser.last_row(), 10);
    assert!(!browser.has_previous());
    assert!(browser.has_next());
    assert!(
        browser.page_footer().contains("rows 1-10 of 62"),
        "{}",
        browser.page_footer()
    );
}

#[test]
fn the_footer_counts_the_answer_not_the_page() {
    // `of N` is the whole answer under the filter. A footer that said 10
    // would make every page look like the last one.
    let browser = browser();
    assert!(
        !browser.page_footer().contains("of 10"),
        "the footer counts the page as if it were the table: {}",
        browser.page_footer()
    );
}

#[test]
fn the_last_page_is_partial_and_offers_no_next_page() {
    let browser = Browser {
        page: 6,
        ..browser()
    };
    assert_eq!(browser.first_row(), 61);
    assert_eq!(browser.last_row(), 62);
    assert!(browser.has_previous());
    assert!(
        !browser.has_next(),
        "the last page offers a next page that does not exist"
    );
    let footer = browser.page_footer();
    assert!(footer.contains("rows 61-62 of 62"), "{footer}");
    assert!(!footer.contains("[n] next"), "{footer}");
    assert!(footer.contains("[p] prev"), "{footer}");
}

#[test]
fn a_page_past_the_end_says_no_rows_match_rather_than_zeroes() {
    // A page number beyond the answer must not draw "rows 71-70 of 62".
    let browser = Browser {
        page: 9,
        ..browser()
    };
    assert_eq!(browser.last_row(), 0);
    assert!(!browser.has_next());
}

#[test]
fn an_empty_answer_says_so() {
    let browser = Browser {
        total: Some(0),
        ..Browser::new("shop", "products")
    };
    let footer = browser.page_footer();
    assert!(footer.contains("no rows match"), "{footer}");
    assert!(!footer.contains("rows 1-0"), "{footer}");
}

#[test]
fn the_sort_and_the_filter_are_named_in_the_footer() {
    // An operator looking at a grid has to be able to tell "this is the
    // table" from "this is part of the table, in this order, for this
    // reason".
    let browser = Browser {
        sort: Some(("stock".to_string(), dg::views::SortDirection::Descending)),
        filter: Some("stock < 20".to_string()),
        total: Some(4),
        ..Browser::new("shop", "products")
    };
    let footer = browser.page_footer();
    assert!(footer.contains("sorted by stock desc"), "{footer}");
    assert!(footer.contains("filter: stock < 20"), "{footer}");
    assert!(footer.contains("rows 1-4 of 4"), "{footer}");
}

#[test]
fn the_outcome_draws_the_rows_with_the_browsers_footer() {
    let outcome = browser_outcome(&browser(), &page());
    assert!(outcome.error.is_none());
    assert_eq!(outcome.headline, "products in shop");
    let grid = outcome.table.expect("the outcome holds the grid");
    // The table's own columns and values.
    let columns: Vec<&str> = grid
        .columns()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(columns, vec!["id", "sku", "stock"]);
    assert_eq!(grid.rows().len(), 3);
    // And the footer is the browser's, not the grid's.
    let footer = grid.footer_text().expect("the grid has a footer");
    assert!(
        footer.contains("rows 1-10 of 62"),
        "the grid does not carry the browser's footer: {footer}"
    );
}

#[test]
fn a_refused_page_reads_like_any_other_refusal() {
    let refused = json!({"error": {"code": "database_not_found", "message": "no such database"}});
    let outcome = browser_outcome(&browser(), &refused);
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

#[test]
fn the_count_is_read_from_the_query_document() {
    let counted = json!({"columns": ["count(*)"], "rows": [[62]], "rows_affected": 0});
    assert_eq!(dg::tui::count_total(&counted), Some(62));
    // A malformed answer is no count, not a panic.
    assert_eq!(dg::tui::count_total(&json!({"rows": []})), None);
    assert_eq!(dg::tui::count_total(&json!({})), None);
}

/// Draw one frame and return the screen as text.
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
fn the_browser_is_drawn_with_its_rows_and_its_footer() {
    let mut app = app();
    app.browser = Some(browser());
    app.outcome = Some(browser_outcome(&browser(), &page()));
    let text = screen(&mut app, 120, 40);
    for expected in ["products in shop", "sku", "AAA", "rows 1-10 of 62"] {
        assert!(
            text.contains(expected),
            "the browser does not draw {expected:?}:\n{text}"
        );
    }
}

#[test]
fn n_and_p_move_the_page_and_queue_one_reread() {
    let mut app = app();
    app.browser = Some(browser());
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(
        app.browser.as_ref().map(|browser| browser.page),
        Some(1),
        "n did not move to the next page"
    );
    assert_eq!(
        app.take_pending(),
        Some(Invocation::RefreshRows),
        "moving the page did not queue the re-read"
    );
    app.on_key(key(KeyCode::Char('p')));
    assert_eq!(
        app.browser.as_ref().map(|browser| browser.page),
        Some(0),
        "p did not move back"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}

#[test]
fn the_page_cannot_be_moved_past_either_end() {
    let mut app = app();
    // On the first page, `p` does nothing at all -- not even a re-read.
    app.browser = Some(browser());
    app.on_key(key(KeyCode::Char('p')));
    assert_eq!(app.browser.as_ref().map(|browser| browser.page), Some(0));
    assert!(
        app.take_pending().is_none(),
        "p on the first page queued a pointless re-read"
    );
    // On the last page, `n` does nothing either.
    app.browser = Some(Browser {
        page: 6,
        ..browser()
    });
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(app.browser.as_ref().map(|browser| browser.page), Some(6));
    assert!(app.take_pending().is_none());
}

#[test]
fn s_leaves_for_the_tables_structure() {
    // The browser's own door to the structure: no palette, no retyping.
    let mut app = app();
    app.browser = Some(browser());
    app.on_key(key(KeyCode::Char('s')));
    assert_eq!(
        app.take_pending(),
        Some(Invocation::Schema {
            name: "shop".to_string(),
            table: Some("products".to_string())
        }),
        "s did not open the table's structure"
    );
    assert!(
        app.browser.is_none(),
        "the browser stayed open over the structure"
    );
}

#[test]
fn esc_leaves_the_browser_without_quitting_the_view() {
    let mut app = app();
    app.browser = Some(browser());
    app.on_key(key(KeyCode::Esc));
    assert!(app.browser.is_none(), "Esc did not leave the browser");
    assert!(
        !app.quit,
        "Esc quit the whole view instead of leaving the browser"
    );
}

#[test]
fn the_filter_is_typed_and_applied_to_the_same_table() {
    // The condition is edited in its own prompt, and Enter re-reads the
    // table already on screen rather than running a new action.
    let mut app = app();
    app.browser = Some(browser());
    app.on_key(key(KeyCode::Char('f')));
    assert_eq!(app.mode, Mode::Filter, "f did not open the filter");
    for character in "stock < 40".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Normal, "Enter did not close the filter");
    let browser = app.browser.clone().expect("the browser is still open");
    assert_eq!(browser.filter.as_deref(), Some("stock < 40"));
    assert_eq!(
        browser.page, 0,
        "a new filter kept the old page, which now means a different set"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}

#[test]
fn an_empty_filter_clears_it() {
    let mut app = app();
    app.browser = Some(Browser {
        filter: Some("stock < 40".to_string()),
        ..browser()
    });
    app.on_key(key(KeyCode::Char('f')));
    // The prompt opens with the current condition, so it is edited.
    for _ in 0.."stock < 40".len() {
        app.on_key(key(KeyCode::Backspace));
    }
    app.on_key(key(KeyCode::Enter));
    let browser = app.browser.clone().expect("the browser is still open");
    assert!(
        browser.filter.is_none(),
        "an empty filter did not clear the condition"
    );
}

/// The sort column is chosen by name, not assumed.
///
/// It used to cycle off the first column, which on a typical table is
/// the id -- the rowid alias -- so the "sort" reordered nothing and every
/// other column was unreachable from the view. The operator names the
/// column instead, and a name that is not a column is refused by the
/// statement builder rather than interpolated into SQL.
#[test]
fn the_sort_column_is_chosen_by_name() {
    let mut app = app();
    app.browser = Some(browser());
    app.outcome = Some(browser_outcome(&browser(), &page()));
    app.on_key(key(KeyCode::Char('o')));
    assert_eq!(app.mode, Mode::Sort, "o did not open the sort prompt");
    // Type a column that is NOT the first one.
    for character in "stock".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Normal, "Enter did not close the prompt");
    let sorted = app.browser.clone().expect("the browser is open");
    assert_eq!(
        sorted.sort,
        Some(("stock".to_string(), dg::views::SortDirection::Ascending)),
        "the named column was not the one sorted"
    );
    assert_eq!(
        sorted.page, 0,
        "a new sort kept the old page, which now holds different rows"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
}

/// A sort direction can be given, and an empty answer clears the sort.
#[test]
fn a_sort_direction_is_optional_and_empty_clears_it() {
    let mut app = app();
    app.browser = Some(browser());
    app.outcome = Some(browser_outcome(&browser(), &page()));
    app.on_key(key(KeyCode::Char('o')));
    for character in "stock:desc".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.browser.as_ref().and_then(|b| b.sort.clone()),
        Some(("stock".to_string(), dg::views::SortDirection::Descending)),
        "the :desc direction was not taken"
    );
    // An empty answer clears it: the table goes back to its own order.
    // The prompt opens with the current sort, so it is edited down to
    // empty rather than typed from scratch.
    app.on_key(key(KeyCode::Char('o')));
    assert_eq!(
        app.input, "stock:desc",
        "the prompt did not open with the current sort to edit"
    );
    for _ in 0.."stock:desc".len() {
        app.on_key(key(KeyCode::Backspace));
    }
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.browser.as_ref().and_then(|b| b.sort.clone()).is_none(),
        "an empty sort answer did not clear the sort"
    );
}

/// A column that is not in the table is refused by the shared builder.
///
/// The view passes the name through; the builder is what refuses it,
/// against the schema's real columns, so the refusal is the same one the
/// CLI gets.
#[test]
fn a_sort_on_a_missing_column_is_refused_by_the_builder() {
    let structure = json!({
        "database": "shop",
        "tables": [{
            "name": "products", "rows": 3,
            "columns": [{"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}],
            "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE products (id INTEGER PRIMARY KEY)"
        }]
    });
    let refused = dg::views::rows_query(
        &structure,
        "products",
        0,
        Some(("stock", dg::views::SortDirection::Ascending)),
        None,
    );
    assert_eq!(
        refused.unwrap_err(),
        dg::views::RowsRefusal::NoSuchColumn("stock".to_string()),
        "a column the table does not have reached the statement"
    );
}

/// The arrow keys and the wheel move a page.
///
/// A browser has pages, not a scrollbar: the arrows used to fall through
/// to the dashboard's scroll, which moved something the operator was not
/// looking at.
#[test]
fn the_arrow_keys_and_the_wheel_move_a_page() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = app();
    app.browser = Some(browser());
    app.on_key(key(KeyCode::Down));
    assert_eq!(
        app.browser.as_ref().map(|b| b.page),
        Some(1),
        "the down arrow did not move a page"
    );
    assert_eq!(app.take_pending(), Some(Invocation::RefreshRows));
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.browser.as_ref().map(|b| b.page), Some(0));
    app.take_pending();

    // The wheel: same direction, same result.
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.browser.as_ref().map(|b| b.page),
        Some(1),
        "the wheel did not move a page"
    );
    app.take_pending();
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.browser.as_ref().map(|b| b.page), Some(0));
    // Without a browser the wheel still scrolls the open tab.
    app.browser = None;
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        app.scroll.databases > 0,
        "the wheel stopped scrolling the dashboard"
    );
    let _ = MouseButton::Left;
}

/// An unread count is not an empty answer.
///
/// The count and the page are separate reads. When the count fails the
/// page is still shown, and the footer must say the total is unknown --
/// "no rows match" would deny rows sitting directly above it.
#[test]
fn an_unread_count_says_so_rather_than_no_rows_match() {
    let unknown = Browser {
        total: None,
        ..Browser::new("shop", "products")
    };
    let footer = unknown.page_footer();
    assert!(
        !footer.contains("no rows match"),
        "an unread count was drawn as an empty answer: {footer}"
    );
    assert!(
        !footer.contains(" of "),
        "an unread count was invented: {footer}"
    );
    assert!(
        footer.contains("rows from 1"),
        "the footer does not say where the page starts: {footer}"
    );
    // And no next page is promised from a total nobody read.
    assert!(
        !unknown.has_next(),
        "a next page was promised without a total"
    );
}

/// A page past the end says so instead of drawing `rows 91-0 of 60`.
///
/// The footer is the view's claim about what the table holds; a range
/// that runs backwards reads as a fault in the table rather than as
/// "there is nothing this far".
#[test]
fn a_page_past_the_end_does_not_draw_a_backwards_range() {
    let past = Browser {
        page: 9,
        ..browser()
    };
    let footer = past.page_footer();
    assert!(
        !footer.contains("91-0"),
        "the footer drew a backwards range: {footer}"
    );
    assert!(
        footer.contains("no rows this far"),
        "the footer does not say the page is past the end: {footer}"
    );
    assert!(
        footer.contains("62"),
        "the footer does not say how many rows there are: {footer}"
    );
}

/// An opened table takes the screen, not a note under the dashboard.
///
/// It used to be drawn in the same small "last action" pane a one-shot
/// answer gets, under a dashboard that kept the rest of the screen --
/// so "open this table" read as information ABOUT the table rather than
/// as the table. Opening something means it is what you are looking at.
#[test]
fn an_opened_table_takes_the_whole_screen() {
    let mut app = app();
    app.browser = Some(browser());
    app.outcome = Some(browser_outcome(&browser(), &page()));
    let text = screen(&mut app, 130, 45);
    // The pane is the opened table, not a "last action" note.
    assert!(
        text.contains("opened"),
        "the opened table is not labelled as opened:\n{text}"
    );
    assert!(
        !text.contains("last action"),
        "an opened table is still drawn as a last-action note:\n{text}"
    );
    // The dashboard's own table is not on screen: the table IS the screen.
    assert!(
        !text.contains("database") || !text.contains("rows read*"),
        "the dashboard is still drawn around the opened table:\n{text}"
    );
    // And its rows and footer are there.
    assert!(text.contains("rows 1-10 of 62"), "{text}");
    assert!(text.contains("AAA"), "the rows are not drawn:\n{text}");
}

/// Esc puts the dashboard back.
#[test]
fn leaving_the_browser_returns_the_dashboard() {
    let mut app = app();
    app.browser = Some(browser());
    app.outcome = Some(browser_outcome(&browser(), &page()));
    let opened = screen(&mut app, 130, 45);
    assert!(opened.contains("opened"));
    app.on_key(key(KeyCode::Esc));
    assert!(app.browser.is_none(), "Esc did not leave the browser");
}
