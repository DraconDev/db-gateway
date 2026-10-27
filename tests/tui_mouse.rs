//! Mouse gestures in the terminal view, drawn on a test terminal.
//!
//! The view is drawn with ratatui's `TestBackend`, so a genuine frame is
//! painted before any click. That matters: the click handler resolves cells
//! against the geometry the drawing code recorded, so a test that skipped the
//! draw would be asserting against an empty layout rather than the one an
//! operator sees. Each test therefore draws, reads where a thing really
//! landed, and clicks that cell.

use dg::tui::actions::Invocation;
use dg::tui::{App, Focus, Mode};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{MouseButton, MouseEvent, MouseEventKind},
};
use serde_json::{Value, json};

fn status() -> Value {
    json!({
        "version": "0.2.0",
        "uptime_secs": 3600,
        "admission": {"acquire_timeout_ms": 250, "read_permits": 4, "write_permits": 2},
        "requests": {
            "total": 120, "capacity_exceeded": 1, "query_deadline_exceeded": 0,
            "internal_error": 0, "recent_per_second": [1, 2, 3], "recent_span_secs": 3,
            "wait_p50_us": 500, "wait_p95_us": 1000, "wait_samples": 4
        },
        "database_counts": {"live": 2, "tombstoned": 0},
        "databases": {
            "demo": {
                "rows_read": 10, "rows_written": 4, "queue_waits": 2, "queue_wait_ms": 3,
                "queue_wait_max_us": 900_000, "queue_wait_p95_us": 1000,
                "queue_timeouts": 0, "last_write": 1_700_000_000
            }
        }
    })
}

fn backups() -> Value {
    json!({"databases": [{
        "database": "demo", "sets": [{"name": "20261021T070000Z",
        "timestamp": "20261021T070000Z", "databases": ["demo"], "has_registry": true,
        "coverage_start": 1_700_000_000, "coverage_end": 1_700_000_100}],
        "per_database": {"demo": {"state": "fresh", "newest_set": "20261021T070000Z", "age_secs": 3600}}
    }]})
}

fn keys() -> Value {
    json!({"keys": [
        {"name": "owner", "role": "owner", "databases": []},
        {"name": "robot", "role": "database_read_only", "databases": ["demo"]}
    ]})
}

fn list() -> Value {
    json!({"databases": ["demo", "other"]})
}

fn app() -> App {
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.status = Some(status());
    app.backups = Some(backups());
    app.keys = Some(keys());
    app.list = Some(list());
    app.connected = true;
    app
}

/// Draw one frame at a fixed size, returning the text and the terminal.
fn frame(app: &mut App, width: u16, height: u16) -> (String, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|f| dg::tui::draw::draw(f, app))
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
    (lines.join("\n"), terminal)
}

fn click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    }
}

/// A pointer that moved to a cell.
fn move_to(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    }
}

fn wheel_events(kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: 10,
        row: 10,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    }
}

/// Find the column where `needle` starts on the row that holds the tab strip.
///
/// The column is counted in cells, not bytes: the active tab is marked with
/// `▸`, which is three bytes in UTF-8 but occupies one cell. Using the byte
/// offset would aim the click up to two cells right of the label it meant,
/// which is how this helper first reported a correct separator as a bug.
fn column_of(text: &str, needle: &str) -> Option<(u16, u16)> {
    use unicode_width::UnicodeWidthStr;
    for (row, line) in text.lines().enumerate() {
        if let Some(byte) = line.find(needle) {
            let cells = line[..byte].width() as u16;
            return Some((cells, row as u16));
        }
    }
    None
}

/// A click on a tab's label switches to that tab.
///
/// This is the gesture a tab bar promises: the label is the target, and
/// clicking it does what the number key does. The test reads the column from
/// the rendered frame rather than assuming one, so it keeps working if the
/// strip is restyled or a tab gains a count.
#[test]
fn clicking_a_tab_label_switches_to_that_tab() {
    let mut app = app();
    assert_eq!(app.focus, Focus::Databases, "starts on databases");
    frame(&mut app, 120, 40);

    for (label, expected) in [
        ("2 service", Focus::Service),
        ("3 backups", Focus::Backups),
        ("4 keys", Focus::Keys),
        ("1 databases", Focus::Databases),
    ] {
        // Redraw first: the geometry comes from the frame that was painted.
        let (text, _t) = frame(&mut app, 120, 40);
        let (column, row) = column_of(&text, label)
            .unwrap_or_else(|| panic!("the strip does not show {label:?}:\n{text}"));
        app.on_mouse(click(column, row));
        assert_eq!(
            app.focus, expected,
            "clicking {label:?} at column {column} should switch to {expected:?}"
        );
    }
}

/// A click on the strip but between labels does nothing.
///
/// The separators between labels are not targets; a click that lands on one
/// must not jump to a neighbouring tab by accident.
#[test]
fn clicking_between_labels_leaves_the_tab_alone() {
    let mut app = app();
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) =
        column_of(&text, "│").unwrap_or_else(|| panic!("the strip has no separator:\n{text}"));
    let before = app.focus;
    app.on_mouse(click(column, row));
    assert_eq!(
        app.focus, before,
        "a click on a separator must not switch tabs"
    );
}

/// The wheel scrolls the open tab, and scrolls no further than the top.
#[test]
fn the_wheel_scrolls_the_open_tab_and_stops_at_the_top() {
    let mut app = app();
    frame(&mut app, 120, 40);

    app.on_mouse(wheel_events(MouseEventKind::ScrollDown));
    assert!(
        app.scroll.databases > 0,
        "a wheel notch down should move the open tab"
    );

    // Scrolling up at the top must clamp rather than wrap to 65535, which is
    // what an unchecked u16 subtraction would do.
    app.scroll.databases = 0;
    for _ in 0..5 {
        app.on_mouse(wheel_events(MouseEventKind::ScrollUp));
    }
    assert_eq!(
        app.scroll.databases, 0,
        "scrolling up at the top must stay at the top, not wrap"
    );
}

/// The wheel only moves the tab that is open.
#[test]
fn the_wheel_scrolls_only_the_open_tab() {
    let mut app = app();
    app.focus = Focus::Keys;
    frame(&mut app, 120, 40);
    app.on_mouse(wheel_events(MouseEventKind::ScrollDown));
    assert!(app.scroll.keys > 0, "the open tab should scroll");
    assert_eq!(app.scroll.databases, 0, "a closed tab must not scroll");
    assert_eq!(app.scroll.service, 0, "a closed tab must not scroll");
    assert_eq!(app.scroll.backups, 0, "a closed tab must not scroll");
}

/// A click while a dialogue is open belongs to the dialogue.
///
/// The palette and the prompt are drawn over the view; a click on the strip
/// underneath must not change tabs and leave the operator typing into an
/// action they did not choose.
#[test]
fn a_click_is_ignored_while_a_dialogue_is_open() {
    let mut app = app();
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) = column_of(&text, "2 service").expect("the strip shows service");

    for mode in [
        Mode::Palette,
        Mode::Prompt {
            kind: dg::tui::actions::ActionKind::CreateDatabase,
        },
    ] {
        app.mode = mode.clone();
        let before = app.focus;
        app.on_mouse(click(column, row));
        assert_eq!(
            app.focus, before,
            "a click in {mode:?} must not switch tabs underneath the dialogue"
        );
    }
}

/// Mouse gestures cannot reach a destructive action.
///
/// Every action in this view still needs its own keystroke or typed
/// confirmation; a stray click must not be able to start one. This pins the
/// scope of mouse support, so adding a clickable button later has to be a
/// deliberate change rather than an accident.
#[test]
fn a_click_never_starts_an_action() {
    let mut app = app();
    frame(&mut app, 120, 40);
    // Click every cell on the strip and in the body.
    for row in 0..6u16 {
        for column in 0..120u16 {
            app.on_mouse(click(column, row));
            assert!(
                app.take_pending().is_none(),
                "a click at {column},{row} started an action"
            );
        }
    }
}

/// The row under the pointer is marked.
///
/// The pointer says which row a click would take, so the operator can
/// see it before taking it. The mark is the row's own background, so
/// the test reads the background of the cells of the row the pointer
/// is over, and of the row above it: one is marked, the other is not.
#[test]
fn the_row_under_the_pointer_is_marked() {
    let mut app = app();
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) = column_of(&text, "demo").unwrap_or_else(|| panic!("no demo row:\n{text}"));
    let (other_column, other_row) =
        column_of(&text, "other").unwrap_or_else(|| panic!("no other row:\n{text}"));

    app.on_mouse(move_to(column, row));
    let (_text, terminal) = frame(&mut app, 120, 40);
    let buffer = terminal.backend().buffer();
    let marked = buffer.cell((column, row)).expect("the cell is there");
    let unmarked = buffer
        .cell((other_column, other_row))
        .expect("the cell is there");
    assert_eq!(
        marked.style().bg,
        dg::tui::theme::hover().bg,
        "the row under the pointer is not marked"
    );
    assert_ne!(
        unmarked.style().bg,
        dg::tui::theme::hover().bg,
        "a row the pointer is not over is marked as well"
    );
}

/// The pointer that leaves the table leaves no mark behind.
///
/// A mark that stays where the pointer has been would say the pointer
/// is somewhere it is not.
#[test]
fn a_pointer_over_the_header_marks_no_row() {
    let mut app = app();
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) =
        column_of(&text, "database").unwrap_or_else(|| panic!("no header:\n{text}"));
    app.on_mouse(move_to(column, row));
    let (_text, terminal) = frame(&mut app, 120, 40);
    let buffer = terminal.backend().buffer();
    let cell = buffer.cell((column, row)).expect("the cell is there");
    assert_ne!(
        cell.style().bg,
        dg::tui::theme::hover().bg,
        "the header is marked as if it were a row"
    );
}

/// Clicking a database opens it.
///
/// The mouse answers the same question the keyboard does: the row that
/// names a database opens that database, and the answer is the
/// database's contents -- the tables it holds.
#[test]
fn clicking_a_database_opens_it() {
    let mut app = app();
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) =
        column_of(&text, "other").unwrap_or_else(|| panic!("no other row:\n{text}"));
    app.on_mouse(click(column, row));
    assert_eq!(
        app.take_pending(),
        Some(Invocation::Schema {
            name: "other".to_string(),
            table: None
        }),
        "the click did not queue the database's opening"
    );
}

/// A click on the keys tab opens nothing.
///
/// The keys are not databases, and a click never reaches a destructive
/// action: the view's rule holds for the mouse as much as for the
/// keyboard.
#[test]
fn clicking_a_credential_opens_nothing() {
    let mut app = app();
    app.focus = Focus::Keys;
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) =
        column_of(&text, "robot").unwrap_or_else(|| panic!("no robot row:\n{text}"));
    app.on_mouse(click(column, row));
    assert_eq!(
        app.mode,
        Mode::Normal,
        "a click on a credential opened something"
    );
    assert!(app.prompt_prefill.is_empty(), "the prompt was filled");
}

/// Clicking a table in the opened database opens that table.
///
/// The database's contents are one row per table, and the grid's first
/// column holds the table's name. Clicking that row opens the table's
/// structure: its columns, their types and their flags.
#[test]
fn clicking_a_table_in_an_opened_database_opens_that_table() {
    let mut app = app();
    // The database was opened: the last action holds its contents.
    let document = json!({
        "database": "demo",
        "tombstoned": false,
        "created_at": 1_700_000_000,
        "tables": [
            {"name": "products", "rows": 60,
             "columns": [{"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}],
             "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE products (id INTEGER PRIMARY KEY)"},
            {"name": "orders", "rows": 12,
             "columns": [{"name": "id", "type": "INTEGER", "not_null": false, "primary_key": true, "default": null}],
             "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE orders (id INTEGER PRIMARY KEY)"}
        ]
    });
    app.outcome = Some(dg::tui::schema_outcome("demo", None, &document));
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) =
        column_of(&text, "orders").unwrap_or_else(|| panic!("no orders row:\n{text}"));
    app.on_mouse(click(column, row));
    assert_eq!(
        app.take_pending(),
        Some(Invocation::Schema {
            name: "demo".to_string(),
            table: Some("orders".to_string())
        }),
        "the click on a table did not open that table"
    );
}

/// A click on a table's own structure opens nothing.
///
/// A structure grid's rows are columns, and a column is not a table.
#[test]
fn clicking_a_column_opens_nothing() {
    let mut app = app();
    let document = json!({
        "database": "demo",
        "tombstoned": false,
        "created_at": 1_700_000_000,
        "tables": [
            {"name": "products", "rows": 60,
             "columns": [{"name": "sku", "type": "TEXT", "not_null": true, "primary_key": false, "default": null}],
             "indexes": [], "foreign_keys": [], "sql": "CREATE TABLE products (sku TEXT NOT NULL)"}
        ]
    });
    app.outcome = Some(dg::tui::schema_outcome("demo", Some("products"), &document));
    let (text, _t) = frame(&mut app, 120, 40);
    let (column, row) = column_of(&text, "sku").unwrap_or_else(|| panic!("no sku row:\n{text}"));
    app.on_mouse(click(column, row));
    assert!(
        app.take_pending().is_none(),
        "a click on a column opened something"
    );
}
