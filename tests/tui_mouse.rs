//! Mouse gestures in the terminal view, drawn on a test terminal.
//!
//! The view is drawn with ratatui's `TestBackend`, so a genuine frame is
//! painted before any click. That matters: the click handler resolves cells
//! against the geometry the drawing code recorded, so a test that skipped the
//! draw would be asserting against an empty layout rather than the one an
//! operator sees. Each test therefore draws, reads where a thing really
//! landed, and clicks that cell.

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

fn wheel_events(kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: 10,
        row: 10,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    }
}

/// Find the column where `needle` starts on the row that holds the tab strip.
fn column_of(text: &str, needle: &str) -> Option<(u16, u16)> {
    for (row, line) in text.lines().enumerate() {
        if let Some(byte) = line.find(needle) {
            // The strip is ASCII, so the byte offset is the cell offset.
            return Some((byte as u16, row as u16));
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
    let (text, _t) = frame(&mut app, 120, 40);

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
    let (column, row) = column_of(&text, "│")
        .unwrap_or_else(|| panic!("the strip has no separator:\n{text}"));
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

    for mode in [Mode::Palette, Mode::Prompt { kind: dg::tui::ActionKind::CreateDatabase }] {
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
    let (text, _t) = frame(&mut app, 120, 40);
    // Click every cell on the strip and in the body.
    for row in 0..6u16 {
        for column in 0..120u16 {
            app.on_mouse(click(column, row));
            assert!(
                app.take_pending().is_none(),
                "a click at {column},{row} started an action:\n{text}"
            );
        }
    }
}
