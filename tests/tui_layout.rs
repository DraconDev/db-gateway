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
        "deployment": {
            "host": "micro2",
            "listen": "127.0.0.1:7432",
            "data_dir": "/var/lib/dg",
            "backup_root": "/var/lib/dg-backups",
            "databases": {
                "demo": {
                    "file": "/var/lib/dg/demo.sqlite",
                    "exists": true,
                    "size_bytes": 40960,
                    "tombstoned": false
                },
                "other": {
                    "file": "/var/lib/dg/other.sqlite",
                    "exists": true,
                    "size_bytes": 12288,
                    "tombstoned": false
                },
                "old_one": {
                    "file": "/var/lib/dg/old_one.sqlite",
                    "exists": false,
                    "size_bytes": 0,
                    "tombstoned": true
                }
            }
        },
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
            "demo": {"state": "fresh", "newest_set": "20261021T070000Z", "age_secs": 3600},
            "other": {"state": "stale", "newest_set": "20261001T070000Z", "age_secs": 1728000}
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

/// A view holding the answers above, freshly built.
///
/// `App` holds the credential and is deliberately not `Clone`, so a test that
/// needs a second view builds another instead of copying one.
fn app_state() -> App {
    app()
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

/// Move to a tab by its number, the way the strip says to.
fn open_tab(app: &mut App, number: char) {
    app.on_key(key(ratatui::crossterm::event::KeyCode::Char(number)));
}

#[test]
fn every_tab_is_named_on_screen_and_holds_its_own_answer() {
    // The view shows one tab at a time. The old layout tiled all four, which
    // squeezed a wide table into a fraction of the width and cut long lists;
    // a tab gives the whole body to the thing being read.
    //
    // The cost of tabbing is that three answers are off screen, so the strip
    // has to pay it: every tab is named whether or not it is showing, and
    // each one really does hold its own answer rather than being a label.
    let mut app = app();
    let strip = screen(&mut app, 120, 40);
    for name in ["databases", "service", "backups", "keys"] {
        assert!(
            strip.contains(name),
            "the tab strip does not name the {name:?} tab:\n{strip}"
        );
    }
    // The databases tab is the one open at rest, and it shows the list.
    assert!(
        strip.contains("demo"),
        "the databases tab is empty:\n{strip}"
    );
    assert!(
        strip.contains("other"),
        "a quiet database is missing:\n{strip}"
    );

    // Each tab, opened by the number the strip prints, shows its own facts.
    let mut app_service = app_state();
    open_tab(&mut app_service, '2');
    let service = screen(&mut app_service, 120, 40);
    assert!(
        service.contains("requests"),
        "the service tab does not show the dashboard:\n{service}"
    );

    let mut app_backups = app_state();
    open_tab(&mut app_backups, '3');
    let backups = screen(&mut app_backups, 120, 40);
    // The freshness table, not the substring "fresh": every screen has
    // "refresh" on the command bar, so a check for the substring passed on
    // a screen that showed no freshness at all.
    assert!(
        backups.contains("newest set"),
        "the backups tab does not show the per-database freshness table:\n{backups}"
    );
    // The states themselves, on the row of the database they belong to: the
    // command bar's "refresh" and the fields' "stale after" are both on
    // every screen, so the state has to be checked against its own row.
    for (database, state) in [("demo", "fresh"), ("other", "stale")] {
        assert!(
            backups
                .lines()
                .any(|line| line.contains(database) && line.contains(state)),
            "the backups tab does not show {database:?} as {state:?}:\n{backups}"
        );
    }
    assert!(
        backups.contains("published sets"),
        "the backups tab does not show the published sets:\n{backups}"
    );

    let mut app_keys = app_state();
    open_tab(&mut app_keys, '4');
    let keys = screen(&mut app_keys, 120, 40);
    assert!(
        keys.contains("robot"),
        "the keys tab does not show the credentials:\n{keys}"
    );

    // And the body gives its width to one answer instead of splitting it, so
    // the database table is wider than it was in the tiled layout.
    assert!(
        !screen(&mut app, 120, 40).contains("robot"),
        "the keys tab is drawn at the same time as the databases tab"
    );
}

#[test]
fn every_tab_is_reachable_from_the_keyboard() {
    // A tab you cannot get to is not a tab. Both the numbers and tab/shift-tab
    // have to work, because the strip advertises both.
    use ratatui::crossterm::event::KeyCode;
    let mut app = app();
    assert_eq!(app.focus, dg::tui::Focus::Databases);

    open_tab(&mut app, '3');
    assert_eq!(app.focus, dg::tui::Focus::Backups);
    open_tab(&mut app, '1');
    assert_eq!(app.focus, dg::tui::Focus::Databases);

    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, dg::tui::Focus::Service, "tab did not advance");
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(
        app.focus,
        dg::tui::Focus::Databases,
        "shift-tab did not go back"
    );
    // Backwards from the first tab wraps to the last, so neither direction
    // can strand the operator.
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(app.focus, dg::tui::Focus::Keys, "shift-tab did not wrap");
}

#[test]
fn the_bar_names_the_keys_that_work() {
    // Discoverability is the reason the bar exists: the palette behind `:` is
    // powerful and invisible, so the keys an operator needs on every screen
    // are printed, with the key distinct from its label.
    let text = screen(&mut app(), 120, 40);
    for hint in ["tabs", "commands", "refresh", "quit", "scroll"] {
        assert!(
            text.contains(hint),
            "the command bar does not offer {hint:?}:\n{text}"
        );
    }
    // The tab hint names numbers, and numbers work (proved above).
    assert!(
        text.contains("1-4"),
        "the bar does not say how to reach the tabs:\n{text}"
    );
}

#[test]
fn a_database_with_no_traffic_is_on_screen_with_zeroes() {
    // "No traffic" and "not a database" must not look the same: the
    // list says which databases exist, the counters say what they did.
    let text = screen(&mut app(), 120, 40);
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
    let text = screen(&mut app(), 80, 24);
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
    let text = screen(&mut app, 120, 40);
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
    let text = screen(&mut app, 120, 40);
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

/// A key press.
fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
}

/// Type a word, one key at a time.
fn type_word(app: &mut App, word: &str) {
    for character in word.chars() {
        app.on_key(key(ratatui::crossterm::event::KeyCode::Char(character)));
    }
}

#[test]
fn a_deletion_asks_for_the_name_twice_and_runs_only_then() {
    // The CLI's rule: the operator repeats the exact name before
    // anything is destroyed. The view asks for the name to choose the
    // action, and again to confirm it — and a name that is not the
    // name does nothing at all.
    let mut app = app();
    app.on_key(key(ratatui::crossterm::event::KeyCode::Char(':')));
    type_word(&mut app, "delete");
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    type_word(&mut app, "demo");
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    // The name is typed once so far: that chose what to delete, and
    // the view asks again.
    assert!(
        app.take_pending().is_none(),
        "the deletion ran on the first mention of the name"
    );

    // A name that is not the name is not consent.
    type_word(&mut app, "dem");
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    assert!(
        app.take_pending().is_none(),
        "the deletion ran on a name that is not the name"
    );
    assert!(
        app.outcome
            .as_ref()
            .and_then(|outcome| outcome.error.as_deref())
            .is_some(),
        "the view did not say why the confirmation was refused"
    );

    // The exact name, and the deletion is armed.
    type_word(&mut app, "demo");
    app.on_key(key(ratatui::crossterm::event::KeyCode::Enter));
    let pending = app
        .take_pending()
        .expect("the exact name did not arm the deletion");
    assert_eq!(
        pending,
        dg::tui::actions::Invocation::DeleteDatabase {
            name: "demo".to_string()
        },
        "the view armed something other than the deletion"
    );
}

#[test]
fn the_databases_tab_says_where_each_database_is_and_whether_it_is_there() {
    // "Is everything live and working" needs the file, not only the
    // counters: a live registry row whose file is gone does no work at
    // all, and its counters would say nothing about that. The traffic
    // table says what a database did; this says where it is.
    let mut app = app();
    let text = screen(&mut app, 191, 40);
    assert!(
        text.contains("locations"),
        "the databases tab does not show where the databases are:\n{text}"
    );
    for file in [
        "/var/lib/dg/demo.sqlite",
        "/var/lib/dg/other.sqlite",
        "/var/lib/dg/old_one.sqlite",
    ] {
        assert!(
            text.contains(file),
            "the databases tab does not show {file:?}:\n{text}"
        );
    }
    // The state belongs to the database it is about. "live" and
    // "tombstoned" are checked against their own rows, because the
    // command bar and the other tables carry those words too.
    for (database, state) in [
        ("demo", "live"),
        ("other", "live"),
        ("old_one", "tombstoned"),
    ] {
        assert!(
            text.lines()
                .any(|line| line.contains(database) && line.contains(state)),
            "the databases tab does not show {database:?} as {state:?}:\n{text}"
        );
    }
}

#[test]
fn the_service_tab_says_where_the_gateway_is() {
    // The client cannot work this out for itself: a gateway reached
    // through a tunnel is a loopback port from the outside, and the
    // machine on the other end is the one holding the data.
    let mut app = app();
    open_tab(&mut app, '2');
    let text = screen(&mut app, 191, 40);
    for fact in [
        "micro2",
        "127.0.0.1:7432",
        "/var/lib/dg",
        "/var/lib/dg-backups",
    ] {
        assert!(
            text.contains(fact),
            "the service tab does not say {fact:?}:\n{text}"
        );
    }
}

#[test]
fn a_gateway_that_reports_no_placement_says_so_rather_than_looking_empty() {
    // An older gateway has no placement to report. Drawing an empty
    // table would read as "there is nowhere they are", which is a
    // different and much worse claim than "this gateway does not say".
    let mut databases_app = app();
    databases_app.status = Some(json!({
        "version": "0.1.0",
        "uptime_secs": 60,
        "database_counts": {"live": 1, "tombstoned": 0},
        "databases": {}
    }));
    let databases = screen(&mut databases_app, 191, 40);
    assert!(
        databases.contains("does not report where its databases live"),
        "the databases tab does not say the gateway does not report its placement:\n{databases}"
    );
    assert!(
        !databases.contains("locations"),
        "an unreported placement is drawn as an empty locations table:\n{databases}"
    );

    let mut service_app = app();
    service_app.status = Some(json!({"version": "0.1.0", "uptime_secs": 60}));
    open_tab(&mut service_app, '2');
    let service = screen(&mut service_app, 191, 40);
    assert!(
        service.contains("not reported by this gateway"),
        "the service tab does not say the gateway does not report its placement:\n{service}"
    );
}

#[test]
fn a_long_database_list_scrolls_and_says_how_many_it_holds() {
    // A list that silently cuts itself is a visibility lie: the
    // operator cannot tell a database that does not exist from one
    // that is below the fold. So the pane counts what it holds, and it
    // scrolls to what it does not show.
    let mut app = app();
    let names: Vec<String> = (0..40).map(|index| format!("db{index:02}")).collect();
    app.list = Some(serde_json::json!({"databases": names}));

    let text = screen(&mut app, 120, 40);
    assert!(
        text.contains("databases (40)"),
        "the pane does not say how many databases it holds:\n{text}"
    );
    assert!(text.contains("db00"), "the first database is not on screen");
    assert!(
        !text.contains("db39"),
        "the whole list is on screen at once, so the count means nothing"
    );

    for _ in 0..60 {
        app.on_key(key(ratatui::crossterm::event::KeyCode::Down));
    }
    let text = screen(&mut app, 120, 40);
    assert!(
        text.contains("db39"),
        "the last database cannot be reached by scrolling:\n{text}"
    );
    assert!(
        !text.contains("db00"),
        "the pane did not scroll at all:\n{text}"
    );
}
