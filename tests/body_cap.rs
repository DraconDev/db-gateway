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
fn oversized(extra: Value) -> Value {
    let mut body = json!({
        "left": "left",
        "right": "right",
        "parent": "parent",
        "name": "fork",
        "pad": "x".repeat(100 * 1024),
    });
    if let (Some(target), Some(source)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    body
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
    let (status, body) = post(&address, &owner, "/v1/diff", &oversized(json!({}))).await;
    assert_eq!(
        status, 413,
        "a body over the 64 KiB cap was accepted by /v1/diff: {body}"
    );

    // `/v1/branch` -- merged from the branch router.
    let (status, body) = post(&address, &owner, "/v1/branch", &oversized(json!({}))).await;
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
        &json!({"sql": "SELECT 1", "args": [], "pad": "x".repeat(100 * 1024)}),
    )
    .await;
    assert_eq!(
        status, 413,
        "a body over the 64 KiB cap was accepted by the query route: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
