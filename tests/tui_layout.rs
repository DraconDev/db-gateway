//! The terminal view's layout, drawn on a test terminal.
//!
//! The view is drawn with ratatui's `TestBackend`, so these tests
//! need no terminal and no gateway: they draw the state the view
//! would hold and read what an operator would read.

use dg::tui::App;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

/// A service answer with two databases, one of them busy.
fn status() -> Value {
    json!({
        "version": "0.2.0",
        "uptime_secs": 3600,
        "admission": {"acquire_timeout_ms": 250, "read_permits": 4, "write_permits": 2},
        "requests": {
            "total": 120,
            "capacity_exceeded": 1,
            "query_deadline_exceeded": 0,
            "internal_error": 0,
            "recent_per_second": [1, 2, 3],
            "recent_span_secs": 3,
            "wait_p50_us": 500,
            "wait_p95_us": 1000,
            "wait_samples": 4
        },
        "database_counts": {"live": 2, "tombstoned": 0},
        "databases": {
            "demo": {
                "rows_read": 10,
                "rows_written": 4,
                "queue_waits": 2,
                "queue_wait_ms": 3,
                "queue_wait_max_us": 900_000,
                "queue_wait_p95_us": 1000,
                "queue_timeouts": 0,
                "last_write": 1_700_000_000
            },
            "other": {
                "rows_read": 1,
                "rows_written": 0,
                "queue_waits": 0,
                "queue_wait_ms": 0,
                "queue_wait_max_us": 0,
                "queue_wait_p95_us": 0,
                "queue_timeouts": 0,
                "last_write": null
            }
        }
    })
}

/// A backup answer: one database, one fresh set.
fn backups() -> Value {
    json!({
        "backup_root": "/var/lib/dg-backups",
        "stale_after_secs": 86400,
        "sets": [{
            "timestamp": "20261021T070000Z",
            "databases": ["demo"],
            "has_registry": true,
            "coverage_start": 1_700_000_000,
            "coverage_end": 1_700_000_100
        }],
        "per_database": {
            "demo": {"state": "fresh", "newest_set": "20261021T070000Z", "age_secs": 3600}
        }
    })
}

/// A credential answer: the owner and one scoped key.
fn keys() -> Value {
    json!({"keys": [
        {"name": "owner", "role": "owner", "databases": []},
        {"name": "robot", "role": "database_read_only", "databases": ["demo"]}
    ]})
}

/// The database list.
fn list() -> Value {
    json!({"databases": ["demo", "other"]})
}

/// A view holding the answers above.
fn app() -> App {
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.status = Some(status());
    app.backups = Some(backups());
    app.keys = Some(keys());
    app.list = Some(list());
    app.connected = true;
    app
}

/// The text on a test terminal.
fn screen(app: &App, width: u16, height: u16) -> String {
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
fn the_view_shows_the_service_its_databases_its_backups_and_its_keys_at_once() {
    // The whole point of the view: the four answers an operator needs
    // are on one screen, not in four commands.
    let text = screen(&app(), 120, 40);
    for expected in [
        "demo",      // the database
        "other",     // the database with no traffic
        "1.0 ms",    // this database's own wait p95
        "fresh",     // the backup state
        "robot",     // a scoped credential
        "connected", // the gateway answered
        "requests",  // the service dashboard
        "databases", // the pane names
        "service",
        "backups",
        "keys",
    ] {
        assert!(
            text.contains(expected),
            "the view does not show {expected:?}:\n{text}"
        );
    }
}

#[test]
fn a_database_with_no_traffic_is_on_screen_with_zeroes() {
    // "No traffic" and "not a database" must not look the same: the
    // list says which databases exist, the counters say what they did.
    let text = screen(&app(), 120, 40);
    let line = text
        .lines()
        .find(|line| line.contains("other"))
        .unwrap_or_else(|| panic!("the second database is not on screen:\n{text}"));
    assert!(
        line.contains("never"),
        "a database that was never written to does not say so: {line}"
    );
}

#[test]
fn the_view_fits_a_small_terminal_without_losing_the_databases() {
    // A terminal of eighty columns is the narrowest one an operator
    // is likely to have. The layout has to survive it.
    let text = screen(&app(), 80, 24);
    assert!(
        text.contains("demo"),
        "the database list is lost on a small terminal:\n{text}"
    );
    assert!(
        text.contains("databases"),
        "the panes lose their names on a small terminal:\n{text}"
    );
}

#[test]
fn an_unreachable_gateway_says_so_rather_than_looking_healthy() {
    let mut app = app();
    app.connected = false;
    app.error = Some("transport failed; the gateway is unavailable".to_string());
    let text = screen(&app, 120, 40);
    assert!(
        text.contains("unreachable"),
        "the view does not say the gateway is unreachable:\n{text}"
    );
    assert!(
        text.contains("transport failed"),
        "the view does not say what went wrong:\n{text}"
    );
}

#[test]
fn the_palette_lists_the_actions_and_the_one_that_is_chosen() {
    let mut app = app();
    app.mode = dg::tui::Mode::Palette;
    app.filter = "key".to_string();
    app.highlighted = 0;
    let text = screen(&app, 120, 40);
    assert!(
        text.contains("issue key"),
        "the palette does not list the key actions:\n{text}"
    );
    assert!(
        text.contains("revoke key"),
        "the palette does not list the key actions:\n{text}"
    );
    assert!(
        !text.contains("create database"),
        "the palette shows an action the filter excludes:\n{text}"
    );
}
