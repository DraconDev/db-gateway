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
        },
        "last_attempt": {
            "ts": "20261021T070000Z",
            "started_at": 1_700_000_000,
            "completed_at": 1_700_000_100,
            "outcome": "ok",
            "set": "20261021T070000Z",
            "databases": 2,
            "step": "publish"
        },
        "last_failure": {
            "ts": "20261020T070000Z",
            "started_at": 1_699_000_000,
            "completed_at": 1_699_000_100,
            "outcome": "error",
            "set": null,
            "databases": 0,
            "step": "export"
        },
        "coverage": {
            "live": 2,
            "in_newest_set": 1,
            "missing": ["other"],
            "registry": true
        },
        "replica": {
            "target": "s3://dg-sandbox",
            "bucket": "dg-sandbox",
            "endpoint": "s3.example.invalid",
            "region": "gra",
            "retention": "168h",
            "snapshot_interval": "24h",
            "databases": ["demo", "other"],
            "registry": true,
            "sidecar": "active",
            "published_at": 1_700_000_100
        },
        "drill": {
            "drilled_at": 1_699_000_000,
            "outcome": "ok",
            "databases": 2,
            "detail": "restored and verified against the live gateway"
        },
        "restore_points": {
            "published_at": 1_700_000_200,
            "window_start": "2026-10-15T14:16:08Z",
            "window_end": "2026-10-20T09:30:41Z",
            "sidecar": "active",
            "databases": {
                "demo": {
                    "oldest": "2026-10-15T14:16:08Z",
                    "newest": "2026-10-16T07:30:03Z",
                    "files": 3,
                    "bytes": 1200
                },
                "other": {
                    "oldest": "2026-10-15T14:16:08Z",
                    "newest": "2026-10-16T07:30:03Z",
                    "files": 3,
                    "bytes": 1200
                }
            }
        },
        "last_restore": {
            "restored_at": 1_700_000_300,
            "database": "demo",
            "timestamp": "2026-10-16T00:00:00Z",
            "outcome": "ok",
            "detail": "restored and verified",
            "tables": 3,
            "rows": 120,
            "size_bytes": 40960,
            "integrity": "ok",
            "path": "/var/lib/dg-restores/20261016000000/demo.sqlite"
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

/// The statement shapes the view holds.
fn queries() -> Value {
    json!({
        "databases": [
            {
                "database": "demo",
                "shapes": [
                    {
                        "shape": "SELECT id, name FROM users WHERE id = ?",
                        "count": 12,
                        "rows_read": 480,
                        "rows_written": 0,
                        "total_micros": 120000,
                        "max_micros": 40000,
                        "first_seen": 1_700_000_000,
                        "last_seen": 1_700_000_100,
                        "principals": {"owner": 12}
                    },
                    {
                        "shape": "INSERT INTO sessions VALUES (?, ?)",
                        "count": 3,
                        "rows_read": 0,
                        "rows_written": 3,
                        "total_micros": 900,
                        "max_micros": 400,
                        "first_seen": 1_700_000_000,
                        "last_seen": 1_700_000_100,
                        "principals": {"robot": 3}
                    }
                ]
            }
        ]
    })
}

/// A view holding the answers above.
fn app() -> App {
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.status = Some(status());
    app.backups = Some(backups());
    app.keys = Some(keys());
    app.list = Some(list());
    app.queries = Some(queries());
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
    for name in [
        "databases",
        "service",
        "backups",
        "keys",
        "queries",
        "locations",
    ] {
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
    // command bar's "refresh" and the fields' "freshness objective" are both
    // on every screen, so the state has to be checked against its own row.
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

    let mut app_locations = app_state();
    open_tab(&mut app_locations, '6');
    let locations = screen(&mut app_locations, 120, 40);
    assert!(
        locations.contains("/var/lib/dg/demo.sqlite"),
        "the locations tab does not show where the databases are:\n{locations}"
    );

    // And the body gives its width to one answer instead of splitting it, so
    // the database table is wider than it was in the tiled layout.
    assert!(
        !screen(&mut app, 120, 40).contains("robot"),
        "the keys tab is drawn at the same time as the databases tab"
    );
}

/// The rule the tabs are built on: one question per tab, one answer per tab.
///
/// This is the audit made into a gate. The placement used to be a second box
/// under the databases tab's traffic table, which split the body in two and
/// squeezed both answers: the tiling problem in a smaller frame. A tab that
/// shows another tab's answer is the same defect wearing a different hat, and
/// it is invisible in a screenshot unless you know what to look for.
#[test]
fn every_tab_answers_its_own_question_and_only_its_own() {
    let mut app = app();

    // The databases tab: what the databases did.
    let databases = screen(&mut app, 191, 40);
    assert!(
        databases.contains("rows read"),
        "the databases tab does not show the traffic:\n{databases}"
    );
    assert!(
        !databases.contains("/var/lib/dg/demo.sqlite"),
        "the databases tab is answering the locations tab's question:\n{databases}"
    );

    // The locations tab: where the files are.
    let mut locations_app = app_state();
    open_tab(&mut locations_app, '6');
    let locations = screen(&mut locations_app, 191, 40);
    assert!(
        locations.contains("/var/lib/dg/demo.sqlite"),
        "the locations tab does not show the files:\n{locations}"
    );
    assert!(
        !locations.contains("rows read"),
        "the locations tab is answering the databases tab's question:\n{locations}"
    );

    // The service tab: the gateway's own facts.
    let mut service_app = app_state();
    open_tab(&mut service_app, '2');
    let service = screen(&mut service_app, 191, 40);
    assert!(
        service.contains("requests"),
        "the service tab does not show the dashboard:\n{service}"
    );
    assert!(
        !service.contains("rows read"),
        "the service tab repeats the databases tab's traffic:\n{service}"
    );

    // The keys tab: the credentials.
    let mut keys_app = app_state();
    open_tab(&mut keys_app, '4');
    let keys = screen(&mut keys_app, 191, 40);
    assert!(keys.contains("robot"), "the keys tab is empty:\n{keys}");
    assert!(
        !keys.contains("rows read"),
        "the keys tab repeats the databases tab's traffic:\n{keys}"
    );
}

/// The tabs are written down once, and the six places that used to write them
/// down are all built from that one list.
///
/// The drift is not hypothetical: the command bar said "1-4" while a fifth
/// tab was on the screen, because the range was written by hand and only the
/// strip was updated. These assertions are what stop the list from growing
/// apart from itself again.
#[test]
fn the_tabs_are_written_down_once_and_everything_is_built_from_that_list() {
    use dg::tui::Focus;

    // Every tab is in the list, in the order the strip shows them.
    let all = Focus::ALL;
    assert_eq!(
        all.len(),
        6,
        "the list of tabs is not the list the strip draws"
    );
    assert_eq!(all[0], Focus::Databases);
    assert_eq!(all[1], Focus::Service);
    assert_eq!(all[2], Focus::Backups);
    assert_eq!(all[3], Focus::Keys);
    assert_eq!(all[4], Focus::Queries);
    assert_eq!(all[5], Focus::Locations);

    // A tab's number is its place in that list, so the number the strip
    // prints is the number the key opens.
    for (index, focus) in all.iter().enumerate() {
        assert_eq!(focus.index(), index, "{focus:?} is in the wrong place");
        assert_eq!(
            Focus::from_number(index + 1),
            Some(*focus),
            "{focus:?} is not reachable by its own number"
        );
    }
    assert_eq!(Focus::from_number(0), None, "there is no zeroth tab");
    assert_eq!(
        Focus::from_number(all.len() + 1),
        None,
        "a number past the last tab opens nothing"
    );

    // The cycle wraps in both directions, so neither can strand the operator.
    let last = *all.last().expect("there is a last tab");
    assert_eq!(last.next(), Focus::Databases, "the cycle does not wrap");
    assert_eq!(Focus::Databases.previous(), last, "shift-tab does not wrap");
    for (index, focus) in all.iter().enumerate() {
        assert_eq!(focus.next(), all[(index + 1) % all.len()]);
        assert_eq!(focus.previous(), all[(index + all.len() - 1) % all.len()]);
    }
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
    assert_eq!(
        app.focus,
        dg::tui::Focus::Locations,
        "shift-tab did not wrap"
    );
    // And the last tab is reachable by its own number, which is the number
    // the strip prints for it.
    open_tab(&mut app, '6');
    assert_eq!(app.focus, dg::tui::Focus::Locations);
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
    // The tab hint names numbers, and numbers work (proved above). The
    // range has to cover EVERY tab: a bar that says "1-4" when there are
    // five tabs hides the fifth one from the operator, and the hint is
    // the only place the count is written down.
    let tabs = dg::tui::Focus::ALL.len();
    assert!(
        text.contains(&format!("1-{tabs}")),
        "the bar does not say how to reach all {tabs} tabs:\n{text}"
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
fn the_locations_tab_says_where_each_database_is_and_whether_it_is_there() {
    // "Is everything live and working" needs the file, not only the
    // counters: a live registry row whose file is gone does no work
    // at all, and the counters would say nothing about that. The traffic
    // table says what a database did; this says where it is.
    //
    // It has its own tab, because a second box under the traffic table
    // split the body in two and squeezed both answers.
    let mut locations_app = app();
    open_tab(&mut locations_app, '6');
    let text = screen(&mut locations_app, 191, 40);
    assert!(
        text.contains("locations"),
        "the locations tab does not name itself:\n{text}"
    );
    for file in [
        "/var/lib/dg/demo.sqlite",
        "/var/lib/dg/other.sqlite",
        "/var/lib/dg/old_one.sqlite",
    ] {
        assert!(
            text.contains(file),
            "the locations tab does not show {file:?}:\n{text}"
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
            "the locations tab does not show {database:?} as {state:?}:\n{text}"
        );
    }
    // And the databases tab does not answer this tab's question as well.
    let mut databases_app = app();
    let databases = screen(&mut databases_app, 191, 40);
    assert!(
        !databases.contains("/var/lib/dg/demo.sqlite"),
        "the databases tab still draws the placement table:\n{databases}"
    );
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
    // The claim lives on the tab that answers it.
    let mut locations_app = app();
    locations_app.status = Some(json!({
        "version": "0.1.0",
        "uptime_secs": 60,
        "database_counts": {"live": 1, "tombstoned": 0},
        "databases": {}
    }));
    open_tab(&mut locations_app, '6');
    let locations = screen(&mut locations_app, 191, 40);
    assert!(
        locations.contains("does not report where its databases live"),
        "the locations tab does not say the gateway does not report its placement:\n{locations}"
    );
    assert!(
        !locations.contains("/var/lib/dg/demo.sqlite"),
        "an unreported placement is drawn as a table of files:\n{locations}"
    );

    let mut service_app = app();
    service_app.status = Some(json!({"version": "0.1.0", "uptime_secs": 60}));
    open_tab(&mut service_app, '2');
    let service = screen(&mut service_app, 191, 40);
    assert!(
        service.contains("not reported by this gateway"),
        "the service tab does not say the gateway does not report its placement:\n{service}"
    );

    // A status answer that has not arrived is a different thing from a
    // gateway that does not give one: blaming the gateway for a question
    // nobody has answered yet is its own kind of wrong.
    let mut unanswered = app();
    unanswered.status = None;
    open_tab(&mut unanswered, '6');
    let locations = screen(&mut unanswered, 191, 40);
    assert!(
        locations.contains("no answer yet"),
        "the locations tab does not say the placement has not arrived:\n{locations}"
    );
    assert!(
        !locations.contains("does not report where its databases live"),
        "a question nobody has answered yet is blamed on the gateway:\n{locations}"
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

/// The databases tab says how big each database is.
///
/// The traffic columns say what a database has been doing; the size says
/// what it costs to keep. A hosted console puts the two side by side, and
/// an operator watching a disk wants both.
#[test]
fn the_databases_tab_says_how_big_each_database_is() {
    let mut app = app();
    let text = screen(&mut app, 191, 50);
    assert!(
        text.contains("size"),
        "the databases tab has no size column:\n{text}"
    );
    // The two live databases, at the sizes the fixture gives them.
    assert!(
        text.contains("40.0 KiB"),
        "the databases tab does not say how big demo is:\n{text}"
    );
    assert!(
        text.contains("12.0 KiB"),
        "the databases tab does not say how big other is:\n{text}"
    );
}

/// A long per-database table does not hide where the copy is.
///
/// The backups tab draws its summary and its tables in one pane, and the
/// per-database table kept every row it was given. With a few dozen
/// databases the summary had no room left, so the two questions an
/// operator has most reason to ask -- where the off-host copy is, and
/// whether it was ever restored and verified -- were scrolled out of
/// sight, while the rows of databases that had been deleted on purpose
/// filled the screen.
#[test]
fn a_long_per_database_table_does_not_hide_where_the_copy_is() {
    let mut app = app();
    let mut value = backups();
    let mut per_database = serde_json::Map::new();
    for index in 0..30 {
        per_database.insert(
            format!("db{index:02}"),
            json!({"state": "stale", "newest_set": "20261001T070000Z", "age_secs": 1728000}),
        );
    }
    value["per_database"] = json!(per_database);
    app.backups = Some(value);
    open_tab(&mut app, '3');
    let text = screen(&mut app, 191, 50);
    for fact in [
        "dg-sandbox",
        "168h",
        "restored and verified",
        "2026-10-15T14:16:08Z",
    ] {
        assert!(
            text.contains(fact),
            "the backups tab does not say {fact:?}:\n{text}"
        );
    }
}

#[test]
fn the_backups_tab_says_whether_the_copies_are_any_good() {
    // Four questions an operator asks before they trust the copies: did
    // the last run work, what does the newest set carry, where is the
    // off-host copy, and has that copy ever been restored.
    let mut app = app();
    open_tab(&mut app, '3');
    let text = screen(&mut app, 191, 50);
    for fact in [
        "dg-sandbox",
        "s3.example.invalid",
        "gra",
        "168h",
        "publish",
        "export",
    ] {
        assert!(
            text.contains(fact),
            "the backups tab does not say {fact:?}:\n{text}"
        );
    }
    // The state of the sidecar and the drill are shown as words, not as
    // the absence of a line: a configured replica that is not replicating
    // is the failure that looks most like success from a distance.
    assert!(
        text.contains("active"),
        "the backups tab does not say whether the sidecar is running:\n{text}"
    );
    assert!(
        text.contains("restored and verified"),
        "the backups tab does not say what the last drill proved:\n{text}"
    );
    // The newest set does not carry every live database, and the tab
    // says which one is missing rather than only counting.
    assert!(
        text.contains("1 of 2 live databases"),
        "the backups tab does not say what the newest set carries:\n{text}"
    );
    assert!(
        text.contains("other"),
        "the backups tab does not name the database the newest set misses:\n{text}"
    );
}

#[test]
fn a_host_that_publishes_no_copy_status_says_so() {
    // The absence of a published fact is a fact: this host does not
    // report its replica or its drill. It must not be drawn as an empty
    // table or, worse, as nothing at all.
    let mut app = app();
    app.backups = Some(json!({
        "backup_root": "/var/lib/dg-backups",
        "stale_after_secs": 86400,
        "sets": [],
        "per_database": {}
    }));
    open_tab(&mut app, '3');
    let text = screen(&mut app, 191, 50);
    assert!(
        text.contains("this host publishes no off-host copy status"),
        "the backups tab does not say the host publishes no replica status:\n{text}"
    );
    assert!(
        text.contains("this host publishes no restore drill result"),
        "the backups tab does not say the host publishes no drill result:\n{text}"
    );
}

#[test]
fn the_freshness_reference_is_named_as_an_objective_not_as_an_age_threshold() {
    // The contract judges freshness by whether a committed change is
    // covered (observability contract §2), not by wall time. A heading
    // that called the reference "stale after" put a number next to the
    // ages in the table below it and read as a contradiction: a set
    // fourteen minutes old, marked fresh, under a heading that said
    // five minutes. The operator's first thought would be that the view
    // is lying, which is exactly what this screen exists to prevent.
    let mut app = app();
    let mut payload = backups();
    payload["stale_after_secs"] = json!(300);
    payload["per_database"]["demo"]["age_secs"] = json!(864);
    app.backups = Some(payload);
    open_tab(&mut app, '3');
    let text = screen(&mut app, 191, 50);
    assert!(
        text.contains("freshness objective: 5m 0s"),
        "the backups tab does not name the freshness objective:\n{text}"
    );
    assert!(
        !text.contains("stale after"),
        "the backups tab still calls the reference an age threshold:\n{text}"
    );
    // The old set that covers every write is still fresh, and the screen
    // must not contradict itself about that.
    assert!(
        text.lines()
            .any(|line| line.contains("demo") && line.contains("fresh")),
        "the fresh database is not shown as fresh:\n{text}"
    );
}

#[test]
fn the_backups_tab_says_which_moments_the_copy_can_be_taken_back_to() {
    // The window is the claim that matters at 2am: not "there is a
    // backup" but "you can go back to any moment between these two".
    // It is the host's fact, and the host publishes it.
    let mut app = app();
    open_tab(&mut app, '3');
    let text = screen(&mut app, 191, 50);
    assert!(
        text.contains("2026-10-15 14:16 UTC to 2026-10-20 09:30 UTC"),
        "the backups tab does not show the restore window:\n{text}"
    );
    assert!(
        text.contains("point-in-time restore"),
        "the backups tab does not name the restore points:\n{text}"
    );
    // The last restore that was taken, so the window is not just a
    // promise nobody has exercised.
    assert!(
        text.contains("demo at 2026-10-16 00:00 UTC (ok)"),
        "the backups tab does not show the last restore:\n{text}"
    );
}

#[test]
fn a_host_that_publishes_no_restore_points_says_so() {
    // The same rule as every other fact: a missing one is reported in
    // words, never drawn as an empty table that reads like "nothing to
    // restore from" when the truth is "nobody said".
    let mut app = app();
    let mut payload = backups();
    payload["restore_points"] = serde_json::Value::Null;
    payload["last_restore"] = serde_json::Value::Null;
    app.backups = Some(payload);
    open_tab(&mut app, '3');
    let text = screen(&mut app, 191, 50);
    assert!(
        text.contains("this host publishes no restore points"),
        "the backups tab does not say the host publishes no restore points:\n{text}"
    );
    assert!(
        text.contains("this host publishes no restore result"),
        "the backups tab does not say the host publishes no restore result:\n{text}"
    );
}

#[test]
fn the_queries_tab_says_which_statement_is_the_expensive_one() {
    // "Which query is heavy?" is a question only the service can answer:
    // every statement passes through the gateway on its way to SQLite.
    // What it shows is the shape -- the SQL with its literals replaced --
    // so the row names a statement without quoting the data it carried.
    let mut app = app();
    open_tab(&mut app, '5');
    let text = screen(&mut app, 191, 50);
    assert!(
        text.contains("queries"),
        "the queries tab is not named on screen:\n{text}"
    );
    assert!(
        text.contains("SELECT id, name FROM users WHERE id = ?"),
        "the queries tab does not show the statement shape:\n{text}"
    );
    assert!(
        text.contains("480"),
        "the queries tab does not show how many rows it read:\n{text}"
    );
    // And which credential ran it: that is the half a hosted provider
    // structurally cannot give.
    assert!(
        text.contains("robot"),
        "the queries tab does not say which credential ran the statement:\n{text}"
    );
}

#[test]
fn a_narrow_terminal_still_names_every_tab() {
    // The strip promises that every tab is always named, whether or not it
    // is showing. A sixth tab made the strip longer than an eighty-column
    // terminal, so the promise broke exactly where it is most useful --
    // and a tab nobody can see is a tab nobody opens.
    let mut app = app();
    for width in [80, 100, 120, 191] {
        let text = screen(&mut app, width, 40);
        // The strip is the second line: the top bar is the first.
        let strip = text.lines().nth(1).unwrap_or("");
        for name in [
            "databases",
            "service",
            "backups",
            "keys",
            "queries",
            "locations",
        ] {
            assert!(
                strip.contains(name),
                "the strip at {width} columns does not name the {name:?} tab:\n{text}"
            );
        }
    }
}

/// The palette marks the actions that cannot be taken back.
///
/// "delete database" sitting beside "refresh" with the same weight tells
/// the operator that the two are the same kind of thing, and they are
/// not. The marker is a character and not only a colour, so it survives a
/// monochrome terminal.
#[test]
fn the_palette_marks_the_actions_that_cannot_be_taken_back() {
    let mut app = app();
    app.on_key(key(ratatui::crossterm::event::KeyCode::Char(':')));
    let text = screen(&mut app, 120, 40);
    let line_with = |needle: &str| {
        text.lines()
            .find(|line| line.contains(needle))
            .unwrap_or("")
            .to_string()
    };
    // The palette is centred, so the marker is found by where the title
    // starts rather than by a fixed column: it is the last thing before
    // the title.
    let marked = |needle: &str| {
        let line = line_with(needle);
        let index = line.find(needle).unwrap_or(0);
        line[..index].trim_end().ends_with('!')
    };
    for destructive in ["delete database", "revoke key", "rotate owner key"] {
        assert!(
            marked(destructive),
            "the palette does not mark {destructive:?} as destructive:\n{}\n---\n{text}",
            line_with(destructive)
        );
    }
    // A safe action is not marked: the marker means something only when
    // it is not on everything.
    assert!(
        !marked("run sql file"),
        "the palette marks a safe action as destructive:\n{text}"
    );
}

/// The palette keeps a gap between a title and its hint.
///
/// The title column was a fixed sixteen characters, and "rotate owner
/// key" is exactly sixteen, so that row ran its hint straight into its
/// title: "rotate owner keythe old owner key dies...". A collision like
/// that turns two pieces of text into one unreadable word, and the
/// widest title on screen is the one that collides.
#[test]
fn the_palette_keeps_a_gap_between_a_title_and_its_hint() {
    let mut app = app();
    app.on_key(key(ratatui::crossterm::event::KeyCode::Char(':')));
    let text = screen(&mut app, 120, 40);

    for action in dg::tui::actions::CATALOG {
        let needle = action.hint;
        let line = text
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("the palette does not show {needle:?}:\n{text}"));
        let index = line.find(needle).expect("the hint is on the line");
        let before = &line[..index];
        // The title ends the title column, and a space separates it
        // from the hint. Without the space the two words are one word.
        assert!(
            before.ends_with(' '),
            "the palette runs {:?} into its hint without a gap: {line:?}",
            action.title
        );
        // And the title is whole: the hint did not start in the middle
        // of it.
        assert!(
            before.contains(action.title),
            "the palette does not show the whole title {:?}: {line:?}",
            action.title
        );
    }
}

/// A tab cannot be scrolled into nothing.
///
/// Scrolling past the last row used to leave the offset running: ten
/// presses down on a two-row table needed ten presses up to come back,
/// though one press had moved everything. And the text tabs -- the
/// dashboard and the backups -- could be scrolled into an empty screen,
/// which reads as "there is nothing here".
#[test]
fn a_tab_cannot_be_scrolled_into_nothing() {
    let mut app = app();
    // The first frame is what tells the view how much room it has.
    let _ = screen(&mut app, 191, 40);
    for _ in 0..20 {
        app.on_key(key(ratatui::crossterm::event::KeyCode::Down));
        let _ = screen(&mut app, 191, 40);
    }
    let text = screen(&mut app, 191, 40);
    assert!(
        text.contains("demo") && text.contains("other"),
        "the database list scrolled away from the screen:\n{text}"
    );
    assert_eq!(
        app.scroll.databases, 0,
        "the scroll ran past the content even though the whole table fits"
    );
}

/// A message that runs off the end of the bar says that it did.
///
/// The bar is one line and the error is the last thing on it, so a long
/// message is cut. A cut that does not say it is a message half-read.
#[test]
fn a_long_error_is_cut_and_says_so() {
    let mut app = app();
    app.error = Some("a very long problem that will not fit on the bar at all".to_string());
    let text = screen(&mut app, 80, 24);
    let bar = text.lines().last().unwrap_or("");
    assert!(
        bar.contains('…'),
        "the error does not say it was cut:\n{bar}"
    );
    assert!(
        !bar.contains("fit on the bar at all"),
        "the error was not cut at all:\n{bar}"
    );
}
