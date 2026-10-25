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
}

/// Answer a read, or refuse it.
async fn read(State(state): State<Arc<Partial>>, request: Request<axum::body::Body>) -> Response {
    let path = request.uri().path().to_string();
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
