//! The view's own refresh: what it says about the gateway, and how it
//! reads it.
//!
//! The view reads five things every two seconds. Two things about that
//! matter to the operator: whether the view says the gateway is there,
//! and how long the view waits when it is not.
//!
//! The gateway here is a real one, in process: a router that answers some
//! of the five paths and refuses the rest. That is the only way to say
//! what the view does when the gateway answers one thing out of five --
//! which is the case the old rule got wrong.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use dg::tui::App;

/// A gateway that answers some of the reads and refuses the rest.
struct Partial {
    /// The paths that answer.
    answer: Vec<String>,
    /// How many reads are in flight at once.
    in_flight: AtomicUsize,
    /// The most that were ever in flight at once.
    peak: AtomicUsize,
    /// How many times each path was read, so a re-read is observable.
    hits: std::sync::Mutex<std::collections::HashMap<String, usize>>,
}

impl Partial {
    /// How many times a path has been read.
    fn hits(&self, path: &str) -> usize {
        *self
            .hits
            .lock()
            .expect("the counter is not poisoned")
            .get(path)
            .unwrap_or(&0)
    }
}

/// Answer a read, or refuse it.
async fn read(State(state): State<Arc<Partial>>, request: Request<axum::body::Body>) -> Response {
    let path = request.uri().path().to_string();
    *state
        .hits
        .lock()
        .expect("the counter is not poisoned")
        .entry(path.clone())
        .or_insert(0) += 1;
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak.fetch_max(now, Ordering::SeqCst);
    // The other reads need a moment to arrive, or they cannot overlap and
    // the peak says nothing.
    tokio::time::sleep(Duration::from_millis(150)).await;
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    // The contents endpoint is the optional, slower sixth read. This
    // fixture's allow/deny list exercises the five concurrent dashboard
    // reads, so it returns an empty but valid contents document by default.
    if path == "/v1/contents" {
        return (StatusCode::OK, axum::Json(json!({"databases": {}}))).into_response();
    }
    if state.answer.contains(&path) {
        (StatusCode::OK, axum::Json(json!({"version": "0.4.1"}))).into_response()
    } else {
        (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": {"code": "forbidden", "message": "refused"}})),
        )
            .into_response()
    }
}

/// Start a partial gateway and hand back its address and its state.
async fn start(answer: Vec<&str>) -> (String, Arc<Partial>) {
    let state = Arc::new(Partial {
        answer: answer.into_iter().map(str::to_string).collect(),
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        hits: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let router = Router::new().fallback(read).with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the listener binds");
    let address = listener.local_addr().expect("the address is known");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), state)
}

/// The view says the gateway is there when it answered something.
///
/// The rule used to be written down as "fewer than four failures", which
/// was right when the view read four things and wrong the moment a fifth
/// was added. The view then said "unreachable" while the gateway was
/// answering — and the connectivity indicator is the thing an operator
/// trusts when something is wrong.
#[tokio::test]
async fn the_view_says_connected_when_the_gateway_answered_something() {
    let (server, _) = start(vec!["/v1/status"]).await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    assert!(
        app.connected,
        "one answer out of five is an answer: the gateway is there"
    );
    assert!(
        app.error.is_some(),
        "and the four refusals are still reported: {:?}",
        app.error
    );
}

/// And it says so when nothing answered at all.
#[tokio::test]
async fn the_view_says_unreachable_when_nothing_answered() {
    let (server, _) = start(vec![]).await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    assert!(
        !app.connected,
        "no answer at all means the gateway is not there"
    );
    assert!(app.error.is_some(), "and the failures are reported");
}

/// The five reads are made together, not one after another.
///
/// In turn, a gateway that hung cost the operator five read budgets —
/// fifty seconds of a frozen view, because the loop drew nothing until
/// the last read came back. The peak in flight is what says whether the
/// reads overlap, and it is measured by the gateway rather than by the
/// clock, so the test cannot be flaky.
#[tokio::test]
async fn the_reads_are_made_together() {
    let (server, state) = start(vec!["/v1/status"]).await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    let peak = state.peak.load(Ordering::SeqCst);
    assert!(
        peak > 1,
        "the reads were made one after another: the peak in flight was {peak}"
    );
}

/// The bar's error says what happened, rather than listing every failure.
///
/// Five refusals of the same kind are one fact, not five, and the bar has
/// one line for it: a message that runs off the end of the line is a
/// message half-read.
#[tokio::test]
async fn the_error_says_how_many_reads_failed_and_why() {
    let (server, _) = start(vec!["/v1/status"]).await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    let error = app.error.expect("the failures are reported");
    assert!(
        error.starts_with("4 of 5 reads failed:"),
        "the error does not summarise the failures: {error}"
    );
    assert_eq!(
        error.matches("refused").count(),
        1,
        "the error lists every failure instead of saying what happened: {error}"
    );
}

/// One failure is one failure: it is reported as it is.
#[tokio::test]
async fn one_failure_is_reported_as_it_is() {
    let (server, _) = start(vec![
        "/v1/status",
        "/v1/backups",
        "/v1/keys",
        "/v1/databases",
    ])
    .await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    let error = app.error.expect("the failure is reported");
    assert!(
        error.contains("refused") && !error.contains("reads failed"),
        "a single failure is dressed up as a count: {error}"
    );
}

/// A manual refresh re-reads the contents, not just the dashboard.
///
/// The contents are on a slower clock than the traffic counters (30
/// seconds), because counting every row of every database is real work
/// an idle view should not do every two seconds. But `r` is the
/// operator saying "tell me what is true now": a count answered from a
/// cache up to 30 seconds old is exactly the stale figure this view
/// exists to stop showing, so `r` invalidates the clock first.
#[tokio::test]
async fn a_manual_refresh_re_reads_the_contents() {
    let (server, gateway) = start(vec![
        "/v1/status",
        "/v1/backups",
        "/v1/keys",
        "/v1/databases",
    ])
    .await;
    let mut app = App::new(server, "a-credential".to_string());
    // The first refresh reads the contents, because nothing has yet.
    app.refresh().await;
    // The second refresh is inside the 30-second window, and must NOT
    // re-read them: that is the cadence doing its job.
    app.refresh().await;
    assert_eq!(
        gateway.hits("/v1/contents"),
        1,
        "the periodic poll ignored its own cadence"
    );

    // `r` invalidates the clock, so the next refresh reads them again.
    app.on_key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('r'),
        ratatui::crossterm::event::KeyModifiers::NONE,
    ));
    let invocation = app.take_pending().expect("r queued a refresh");
    app.run_invocation(invocation).await;
    assert_eq!(
        gateway.hits("/v1/contents"),
        2,
        "a manual refresh did not re-read the contents"
    );
}

/// Opening a database re-reads the contents too.
///
/// Opening a database is a request to see what is in it, and the counts
/// on the databases tab are part of that answer: they are re-read rather
/// than repeated from a cache.
#[tokio::test]
async fn opening_a_database_re_reads_the_contents() {
    let (server, gateway) = start(vec![
        "/v1/status",
        "/v1/backups",
        "/v1/keys",
        "/v1/databases",
    ])
    .await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    // The stub answers /v1/databases with a fixed body, so the list is
    // set AFTER the refresh: this test is about the click path, not
    // about what that stub returns.
    app.list = Some(json!({"databases": ["shop"]}));
    assert_eq!(gateway.hits("/v1/contents"), 1);

    // A frame is DRAWN first, because the click handler resolves cells
    // against the geometry the drawing code recorded: without a frame
    // there is no layout, the click lands on nothing, and the test would
    // be proving the fallback path rather than the one an operator takes.
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| dg::tui::draw::draw(frame, &mut app))
        .expect("the view draws");

    // The database row, read off the drawn frame rather than assumed.
    let buffer = terminal.backend().buffer();
    let mut hit = None;
    for y in 0..40u16 {
        let line: String = (0..120u16)
            .map(|x| {
                buffer
                    .cell((x, y))
                    .map(|cell| cell.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect();
        if line.contains("shop") && !line.contains("database") {
            hit = Some((line.find("shop").unwrap() as u16, y));
            break;
        }
    }
    let (column, row) = match hit {
        Some(found) => found,
        None => {
            // Show what was drawn, so a failure says why.
            let mut screen = String::new();
            for y in 0..40u16 {
                for x in 0..120u16 {
                    screen.push_str(
                        &buffer
                            .cell((x, y))
                            .map(|cell| cell.symbol().to_string())
                            .unwrap_or_default(),
                    );
                }
                screen.push('\n');
            }
            panic!("the database row was not drawn:\n{screen}");
        }
    };
    app.on_mouse(ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column,
        row,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    });
    let invocation = app
        .take_pending()
        .expect("the click on the database row queued nothing");
    assert_eq!(
        invocation,
        dg::tui::actions::Invocation::OpenDatabase {
            name: "shop".to_string()
        },
        "the click did not queue the database's page"
    );
    app.run_invocation(invocation).await;
    assert_eq!(
        gateway.hits("/v1/contents"),
        2,
        "opening a database did not re-read the contents"
    );
}

struct ContentsGateway {
    reads: AtomicUsize,
    rows: AtomicUsize,
    fail: AtomicBool,
}

async fn contents_gateway(
    State(state): State<Arc<ContentsGateway>>,
    request: Request<axum::body::Body>,
) -> Response {
    let path = request.uri().path();
    if path == "/v1/contents" {
        state.reads.fetch_add(1, Ordering::SeqCst);
        if state.fail.load(Ordering::SeqCst) {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": {"code": "contents_unavailable", "message": "counts refused"}
                })),
            )
                .into_response();
        }
        return (
            StatusCode::OK,
            axum::Json(json!({
                "databases": {
                    "shop": {"tables": 1, "rows": state.rows.load(Ordering::SeqCst)}
                }
            })),
        )
            .into_response();
    }
    if path == "/v1/databases/shop/query" && request.method() == axum::http::Method::POST {
        state.rows.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::OK,
            axum::Json(json!({
                "columns": [], "rows": [], "rows_affected": 1
            })),
        )
            .into_response();
    }
    let body = match path {
        "/v1/status" => json!({"version": "0.9.0"}),
        "/v1/backups" | "/v1/keys" | "/v1/queries" => json!({}),
        "/v1/databases" => json!({"databases": ["shop"]}),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": {"code": "not_found", "message": "no route"}})),
            )
                .into_response();
        }
    };
    (StatusCode::OK, axum::Json(body)).into_response()
}

async fn start_contents_gateway() -> (String, Arc<ContentsGateway>) {
    let state = Arc::new(ContentsGateway {
        reads: AtomicUsize::new(0),
        rows: AtomicUsize::new(4),
        fail: AtomicBool::new(false),
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the listener binds");
    let address = listener.local_addr().expect("the address is known");
    let router = Router::new()
        .fallback(contents_gateway)
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), state)
}

#[tokio::test]
async fn palette_refresh_invalidates_contents_inside_the_cadence_window() {
    let (server, state) = start_contents_gateway().await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    app.refresh().await;
    assert_eq!(state.reads.load(Ordering::SeqCst), 1);

    app.run_invocation(dg::tui::actions::Invocation::Refresh)
        .await;
    assert_eq!(
        state.reads.load(Ordering::SeqCst),
        2,
        "the palette refresh reused counts inside the 30-second window"
    );
}

#[tokio::test]
async fn database_page_keeps_its_rows_browser_page_when_reloading() {
    let (server, _) = start_contents_gateway().await;
    let mut app = App::new(server, "a-credential".to_string());
    app.page = Some(dg::tui::DatabasePage {
        database: "shop".to_string(),
        tables: vec![dg::views::TableStructure {
            name: "products".to_string(),
            rows: 25,
            columns: vec![dg::views::ColumnStructure {
                name: "id".to_string(),
                kind: "INTEGER".to_string(),
                not_null: false,
                primary_key: true,
                default: None,
            }],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            sql: Some("CREATE TABLE products (id INTEGER PRIMARY KEY)".to_string()),
        }],
        selected: 0,
        list_scroll: 0,
    });
    app.browser = Some(dg::tui::Browser {
        total: Some(25),
        ..dg::tui::Browser::new("shop", "products")
    });

    app.on_key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('n'),
        ratatui::crossterm::event::KeyModifiers::NONE,
    ));
    let invocation = app.take_pending().expect("n queues a page read");
    app.run_invocation(invocation).await;
    assert_eq!(
        app.browser.as_ref().map(|browser| browser.page),
        Some(1),
        "reloading the database page discarded its rows-browser page"
    );
}

/// A database page's filtered rows are counted under that filter.
///
/// The database's own structure says how many rows the TABLE has. That is
/// the right total for an unfiltered page and the wrong one for a filtered
/// page: the footer would promise sixty rows for an answer of twenty-four,
/// and paging would disagree with the count it printed.
#[tokio::test]
async fn database_page_counts_rows_under_the_active_filter() {
    async fn answer(
        State(state): State<Arc<FilteredGateway>>,
        request: Request<axum::body::Body>,
    ) -> Response {
        let path = request.uri().path().to_string();
        if path == "/v1/databases/shop/schema" {
            return (
                StatusCode::OK,
                axum::Json(json!({
                    "database": "shop",
                    "tombstoned": false,
                    "created_at": 1_700_000_000,
                    "tables": [{
                        "name": "products",
                        "rows": 60,
                        "columns": [{
                            "name": "stock", "type": "INTEGER",
                            "not_null": false, "primary_key": false, "default": null
                        }],
                        "indexes": [], "foreign_keys": [],
                        "sql": "CREATE TABLE products (stock INTEGER)"
                    }]
                })),
            )
                .into_response();
        }
        if path == "/v1/databases/shop/query" {
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(request.into_body(), 1 << 20)
                    .await
                    .expect("the body is readable"),
            )
            .expect("the body is JSON");
            let sql = body["sql"].as_str().unwrap_or_default().to_string();
            if sql.starts_with("SELECT count(*)") {
                state.counted.fetch_add(1, Ordering::SeqCst);
                return (
                    StatusCode::OK,
                    axum::Json(json!({
                        "columns": ["count(*)"], "rows": [[24]], "rows_affected": 0
                    })),
                )
                    .into_response();
            }
            return (
                StatusCode::OK,
                axum::Json(json!({
                    "columns": ["stock"],
                    "rows": (0..10).map(|i| json!([100 - i])).collect::<Vec<_>>(),
                    "rows_affected": 0
                })),
            )
                .into_response();
        }
        let body = match path.as_str() {
            "/v1/contents" => json!({"databases": {"shop": {"tables": 1, "rows": 60}}}),
            "/v1/status" => json!({"version": "0.9.0"}),
            "/v1/backups" | "/v1/keys" | "/v1/queries" => json!({}),
            "/v1/databases" => json!({"databases": ["shop"]}),
            _ => {
                return (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({"error": {"code": "not_found", "message": "no route"}})),
                )
                    .into_response();
            }
        };
        (StatusCode::OK, axum::Json(body)).into_response()
    }

    struct FilteredGateway {
        counted: AtomicUsize,
    }

    let state = Arc::new(FilteredGateway {
        counted: AtomicUsize::new(0),
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the listener binds");
    let address = listener.local_addr().expect("the address is known");
    let router = Router::new().fallback(answer).with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let mut app = App::new(format!("http://{address}"), "a-credential".to_string());
    app.page = Some(dg::tui::DatabasePage {
        database: "shop".to_string(),
        tables: vec![dg::views::TableStructure {
            name: "products".to_string(),
            rows: 60,
            columns: vec![dg::views::ColumnStructure {
                name: "stock".to_string(),
                kind: "INTEGER".to_string(),
                not_null: false,
                primary_key: false,
                default: None,
            }],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            sql: Some("CREATE TABLE products (stock INTEGER)".to_string()),
        }],
        selected: 0,
        list_scroll: 0,
    });
    app.browser = Some(dg::tui::Browser {
        filter: Some("stock > 60".to_string()),
        ..dg::tui::Browser::new("shop", "products")
    });

    app.run_invocation(dg::tui::actions::Invocation::RefreshRows)
        .await;

    assert_eq!(
        state.counted.load(Ordering::SeqCst),
        1,
        "the filtered page did not count what the filter selects"
    );
    let browser = app.browser.expect("the browser stays open");
    assert_eq!(
        browser.total,
        Some(24),
        "the footer total is the table's size, not the filtered answer's"
    );
    assert_eq!(browser.filter.as_deref(), Some("stock > 60"));
    assert_eq!(browser.page, 0);
}

#[tokio::test]
async fn manual_refresh_clears_a_one_time_key_result() {
    let (server, _) = start_contents_gateway().await;
    let mut app = App::new(server, "a-credential".to_string());
    app.outcome = Some(dg::tui::Outcome {
        headline: "issued robot".to_string(),
        facts: vec![("key".to_string(), "one-time-secret".to_string())],
        table: None,
        error: None,
    });
    app.run_invocation(dg::tui::actions::Invocation::Refresh)
        .await;
    assert!(
        app.outcome.is_none(),
        "manual refresh left the raw one-time key on screen"
    );
}

#[tokio::test]
async fn a_successful_write_refreshes_database_contents_after_the_write() {
    let (server, state) = start_contents_gateway().await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    assert_eq!(
        app.contents.as_ref().unwrap()["databases"]["shop"]["rows"],
        4
    );
    assert_eq!(state.reads.load(Ordering::SeqCst), 1);

    let invocation = dg::tui::actions::ActionKind::Query
        .parse("shop INSERT INTO products VALUES (5)")
        .expect("the write query parses");
    app.run_invocation(invocation).await;

    assert_eq!(
        state.reads.load(Ordering::SeqCst),
        2,
        "the successful database write did not trigger a contents read"
    );
    assert_eq!(
        app.contents.as_ref().unwrap()["databases"]["shop"]["rows"],
        5,
        "the dashboard kept the pre-write count"
    );
}

#[tokio::test]
async fn a_failed_contents_read_shows_unknown_counts_and_retries() {
    let (server, state) = start_contents_gateway().await;
    let mut app = App::new(server, "a-credential".to_string());
    app.refresh().await;
    assert_eq!(
        app.contents.as_ref().unwrap()["databases"]["shop"]["rows"],
        4
    );

    state.fail.store(true, Ordering::SeqCst);
    app.contents_stale();
    app.refresh().await;
    assert_eq!(
        app.contents.as_ref().unwrap()["databases"]["shop"]["rows"],
        Value::Null,
        "failed refresh kept stale counts instead of marking them unknown"
    );
    assert!(app.contents_due(), "the failed read was marked fresh");
    assert!(
        app.error
            .as_deref()
            .is_some_and(|error| error.contains("counts refused")),
        "the failed contents read was hidden: {:?}",
        app.error
    );

    state.fail.store(false, Ordering::SeqCst);
    app.refresh().await;
    assert_eq!(
        app.contents.as_ref().unwrap()["databases"]["shop"]["rows"],
        4,
        "the next refresh did not retry the failed counts read"
    );
    assert_eq!(state.reads.load(Ordering::SeqCst), 3);
}
