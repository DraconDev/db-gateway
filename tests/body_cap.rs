//! The documented 64 KiB request-body cap must hold on EVERY route.
//!
//! `docs/api.md` states the contract in one line: "JSON body over the
//! 64 KiB general cap" is refused with 413 `invalid_request`. The gateway
//! applies that cap with a `DefaultBodyLimit` layer, but the layer is
//! applied to the routes that exist at the moment it is added -- and
//! `Router::merge` inserts another router's routes afterwards without
//! re-applying the outer router's layers. The diff and branch routers
//! are merged that way, so on those two routes the cap did not apply and
//! the body fell back to axum's own default of 2 MiB.
//!
//! This test proves the cap on the routes that were merged (which is
//! where it did not hold) as well as on one that was not, so a future
//! merge cannot quietly widen the envelope again.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

/// Serve a data directory over a real socket with the FULL API router --
/// the diff, branch, audit, db-ops and backup-run routers merged in, the
/// same way `dg serve` builds it.
async fn serve(data_dir: &PathBuf) -> (String, String) {
    let store = dg::store::Store::open(data_dir).unwrap();
    let owner = store.bootstrap().unwrap();
    let store = Arc::new(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let deployment = dg::api::Deployment {
        listen: address,
        host: None,
    };
    let app = dg::router().merge(dg::api::router(store.clone(), deployment));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), owner)
}

async fn post(address: &str, key: &str, path: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{address}{path}"))
        .bearer_auth(key)
        .json(body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// A body over the 64 KiB cap, built from the named fields the route
/// needs plus enough padding to cross the line.
///
/// The padding goes INSIDE a field the route declares: `DiffRequest` and
/// `ForkBody` both deny unknown fields, so a stray key would be refused
/// as a schema error before the size ever mattered -- and a schema
/// refusal would prove nothing about the cap.
fn oversized(route: &str) -> Value {
    let pad = "x".repeat(100 * 1024);
    match route {
        "/v1/diff" => json!({"left": pad, "right": "right"}),
        "/v1/branch" => json!({"parent": pad, "name": "fork"}),
        _ => json!({"sql": pad, "args": []}),
    }
}

/// The 64 KiB general cap holds on the routes that are MERGED into the
/// gateway after the cap layer was applied.
///
/// Before the fix, `/v1/diff` and `/v1/branch` accepted a 100 KiB body:
/// the `DefaultBodyLimit` layer is applied to the routes present when it
/// is added, and `Router::merge` inserts the other router's routes
/// without re-applying the outer router's layers, so the cap fell back to
/// axum's 2 MiB default. A body over the documented cap was accepted
/// instead of refused with 413.
#[tokio::test]
async fn the_general_body_cap_holds_on_the_merged_routers() {
    let dir = std::env::temp_dir().join(format!("dg-bodycap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (address, owner) = serve(&dir).await;

    // `/v1/diff` -- merged from the diff router.
    let (status, body) = post(&address, &owner, "/v1/diff", &oversized("/v1/diff")).await;
    assert_eq!(
        status, 413,
        "a body over the 64 KiB cap was accepted by /v1/diff: {body}"
    );

    // `/v1/branch` -- merged from the branch router.
    let (status, body) = post(&address, &owner, "/v1/branch", &oversized("/v1/branch")).await;
    assert_eq!(
        status, 413,
        "a body over the 64 KiB cap was accepted by /v1/branch: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The cap still holds on a route that was never merged, so the fix is
/// not "remove the cap" but "apply it everywhere".
#[tokio::test]
async fn the_general_body_cap_holds_on_an_unmerged_route() {
    let dir = std::env::temp_dir().join(format!("dg-bodycap2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (address, owner) = serve(&dir).await;

    let (status, body) = post(
        &address,
        &owner,
        "/v1/databases/x/query",
        &oversized("/v1/databases/x/query"),
    )
    .await;
    assert_eq!(
        status, 413,
        "a body over the 64 KiB cap was accepted by the query route: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The per-route caps still win over the general one.
///
/// The import route declares its own 12 MiB cap and the chunk route its
/// own 2 MiB, both with `MethodRouter::layer`, which sits closer to the
/// handler than the router-level layer. Moving the general cap after the
/// merges must not have shrunk those two envelopes.
#[tokio::test]
async fn the_import_route_keeps_its_own_larger_cap() {
    let dir = std::env::temp_dir().join(format!("dg-bodycap3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (address, owner) = serve(&dir).await;

    // 100 KiB: over the 64 KiB general cap, under the import's 12 MiB.
    // The import must NOT refuse it for size -- it will refuse it for
    // another reason (the name does not exist), and the point is that
    // the refusal is not 413.
    let (status, body) = post(
        &address,
        &owner,
        "/v1/databases/imported/import",
        &json!({"data": "x".repeat(100 * 1024)}),
    )
    .await;
    assert_ne!(
        status, 413,
        "the import route lost its own 12 MiB cap: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
