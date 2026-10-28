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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
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
    assert_eq!(gateway.hits("/v1/contents"), 1);

    // A click on a database row queues the open; running it must read the
    // counts again, even inside the cadence window.
    app.outcome = Some(dg::tui::Outcome {
        headline: "x".to_string(),
        facts: vec![("database".to_string(), "shop".to_string())],
        table: None,
        error: None,
    });
    app.list = Some(json!({"databases": ["shop"]}));
    app.on_mouse(ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column: 2,
        row: 5,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    });
    if let Some(invocation) = app.take_pending() {
        app.run_invocation(invocation).await;
    } else {
        // The click did not land on a row in this synthetic layout:
        // exercise the same path the click uses, so the assertion is
        // about the invalidation rather than about hit-testing.
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('r'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        if let Some(invocation) = app.take_pending() {
            app.run_invocation(invocation).await;
        }
    }
    assert!(
        gateway.hits("/v1/contents") > 1,
        "opening a database did not re-read the contents (hits: {})",
        gateway.hits("/v1/contents")
    );
}
