//! One view layer: the CLI and the terminal view read the same facts.
//!
//! The defect this protects against is concrete: a batch the gateway
//! refused was drawn as "not run" on one surface and "rolled back" on
//! another, because each surface read the response on its own. The
//! view layer is where the reading happens now, once, for every
//! surface.

use dg::render::{Theme, write_sections};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

/// A service answer.
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
        "database_counts": {"live": 1, "tombstoned": 0},
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
            }
        }
    })
}

/// The CLI's own drawing of the dashboard, through the shared view.
fn cli_text(status: &Value) -> String {
    let sections = dg::views::service_dashboard(status);
    let mut out = Vec::new();
    write_sections(&mut out, &Theme::fixed(false, true), &sections).expect("a dashboard writes");
    String::from_utf8(out).expect("a dashboard is text")
}

/// The terminal view's drawing of one tab.
fn tui_tab_text(status: &Value, focus: dg::tui::Focus) -> String {
    let mut app = dg::tui::App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.status = Some(status.clone());
    app.list = Some(json!({"databases": ["demo"]}));
    app.connected = true;
    app.focus = focus;
    let backend = TestBackend::new(120, 120);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| dg::tui::draw::draw(frame, &mut app))
        .expect("the view draws");
    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..120 {
        let line: String = (0..120)
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

/// The terminal view's drawing of the same dashboard.
fn tui_text(status: &Value) -> String {
    // The view shows one tab at a time. The service facts are on the service
    // tab; the per-database traffic is on the databases tab. Comparing the
    // CLI's dashboard against the view's means reading the tabs that hold the
    // dashboard's parts, not reading whatever happens to be on screen.
    tui_tab_text(status, dg::tui::Focus::Service)
}

/// Everything the terminal view says about the dashboard: the service tab and
/// the databases tab together.
fn tui_dashboard_text(status: &Value) -> String {
    format!(
        "{}\n{}",
        tui_tab_text(status, dg::tui::Focus::Service),
        tui_tab_text(status, dg::tui::Focus::Databases)
    )
}

#[test]
fn a_section_that_is_only_a_table_does_not_print_an_empty_heading() {
    // The defect this protects: a section whose only block is a table used to
    // print its heading and nothing under it, because `section_lines` draws
    // fields only. The service dashboard ends with exactly such a section
    // (`per-database`), so the pane showed a title over empty space.
    //
    // The fix is to DRAW the table, not to suppress the heading -- suppressing
    // it would trade a dangling title for a missing answer. So the assertion
    // is that the heading, when present, has its table beneath it.
    let status = status();
    let tui = tui_text(&status);
    let headings = tui.matches("per-database").count();
    if headings > 0 {
        // If the title is on screen, the table must be too: a header row and
        // the demo database's traffic belong under it.
        assert!(
            tui.contains("rows read") && tui.contains("demo"),
            "a per-database heading is drawn without its table:\n{tui}"
        );
    }
    // And the sections that need headings keep them.
    for heading in ["activity", "admission"] {
        assert!(
            tui.contains(heading),
            "the view lost the {heading:?} heading:\n{tui}"
        );
    }
    // The dashboard's own headline is not repeated under the tab strip.
    let heading_rows: Vec<&str> = tui
        .lines()
        .map(|line| line.trim())
        .filter(|line| *line == "service")
        .collect();
    assert!(
        heading_rows.is_empty(),
        "the dashboard repeats its own tab name as a heading (found {})\n{tui}",
        heading_rows.len()
    );
}

#[test]
fn the_cli_and_the_view_show_the_same_numbers() {
    // The same facts, read once: the CLI's dashboard and the terminal
    // view's dashboard cannot tell the operator different stories.
    //
    // The view is tabbed, so its story is spread across tabs: the service
    // facts on the service tab, the per-database traffic on the databases
    // tab. Both are read here, because the contract is about the view as a
    // whole and not about one of its tabs.
    let status = status();
    let cli = cli_text(&status);
    let tui = tui_dashboard_text(&status);
    for fact in [
        "0.2.0",  // the version
        "1h 0m",  // the uptime
        "120",    // the request total
        "1.0 ms", // the wait p95
        "10",     // the rows read
    ] {
        assert!(cli.contains(fact), "the CLI does not show {fact:?}:\n{cli}");
        assert!(
            tui.contains(fact),
            "the terminal view does not show {fact:?}:\n{tui}"
        );
    }
    // The worst single wait is the dashboard's own column. It used to be
    // shown by the CLI alone, because the view's database list was a narrow
    // pane with room for fewer figures; the list is a whole tab now, so
    // both surfaces carry every figure.
    assert!(
        cli.contains("900 ms"),
        "the CLI does not show the worst single wait:\n{cli}"
    );
    assert!(
        tui.contains("900 ms"),
        "the terminal view does not show the worst single wait:\n{tui}"
    );
}

#[test]
fn the_database_list_shows_every_database_the_gateway_lists() {
    // A database that has never been used is still a database, and the
    // view says so with zeroes rather than leaving it out.
    let list = json!({"databases": ["demo", "quiet"]});
    let status = status();
    let table = dg::views::databases(&list, &status);
    let mut out = Vec::new();
    table
        .write(&mut out, &Theme::fixed(false, true))
        .expect("a table writes");
    let text = String::from_utf8(out).expect("a table is text");
    assert!(
        text.contains("demo"),
        "the list is missing a database:\n{text}"
    );
    assert!(
        text.contains("quiet"),
        "a database with no traffic is missing from the list:\n{text}"
    );
    let quiet = text
        .lines()
        .find(|line| line.contains("quiet"))
        .unwrap_or_else(|| panic!("the quiet database is not on screen:\n{text}"));
    assert!(
        quiet.contains("never"),
        "a database that was never written to does not say so: {quiet}"
    );
}

#[test]
fn a_refused_batch_reads_the_same_way_on_both_surfaces() {
    // The reading that produced the two false statements — "not run"
    // and "rolled back" — now happens once, in the view layer.
    let statements = vec!["select 1".to_string(), "select 2".to_string()];
    let refused = json!({"error": {"code": "too_many_statements", "message": "too long"}});
    let outcome = dg::views::batch_outcomes(&statements, &refused);
    let mut out = Vec::new();
    outcome
        .table
        .write(&mut out, &Theme::fixed(false, true))
        .expect("a table writes");
    let text = String::from_utf8(out).expect("a table is text");
    assert_eq!(
        text.matches("not run").count(),
        2,
        "a refused batch does not say that nothing ran:\n{text}"
    );
    assert!(
        !text.contains("rolled back"),
        "a refused batch claims a rollback:\n{text}"
    );
    assert!(
        outcome.refused,
        "the shared reading does not know the request was refused"
    );
    assert_eq!(
        outcome.reason().as_deref(),
        Some("too_many_statements: too long"),
        "the refusal does not carry the gateway's reason"
    );
}

#[test]
fn a_lone_statement_that_fails_carries_the_reason() {
    // The gateway names no statement for a batch of one, so the row
    // has to carry the reason: "not run" would be a claim the gateway
    // never made.
    let statements = vec!["select 1".to_string()];
    let failed = json!({"error": {"code": "sql_error", "message": "no such table: t"}});
    let outcome = dg::views::batch_outcomes(&statements, &failed);
    let mut out = Vec::new();
    outcome
        .table
        .write(&mut out, &Theme::fixed(false, true))
        .expect("a table writes");
    let text = String::from_utf8(out).expect("a table is text");
    assert!(
        text.contains("sql_error: no such table: t"),
        "the row does not carry the reason:\n{text}"
    );
    assert!(
        !text.contains("not run"),
        "the row claims the statement did not run:\n{text}"
    );
}
