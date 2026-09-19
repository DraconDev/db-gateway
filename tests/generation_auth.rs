//! Generation-bound credential authorization and replay regression coverage.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dg::{
    api,
    store::{KeyRequest, Store},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn call(
    app: &Router,
    path: &str,
    key: &str,
    body: Value,
    idempotency: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if let Some(idempotency) = idempotency {
        request = request.header("idempotency-key", idempotency);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn purged_name_replacement_rejects_old_scoped_replay_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let owner = store.bootstrap().unwrap();
    store.create("reused").unwrap();
    let old_key = store
        .issue(KeyRequest {
            name: "old_app".into(),
            role: "database_read_write".into(),
            databases: vec!["reused".into()],
        })
        .unwrap();
    let app = api::router(store.clone());

    let (status, body) = call(
        &app,
        "/v1/databases/reused/query",
        &old_key,
        json!({"sql":"CREATE TABLE t (value INTEGER)"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        "/v1/databases/reused/query",
        &old_key,
        json!({"sql":"INSERT INTO t VALUES (1)"}),
        Some("replay-before-purge"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    store.delete("reused", 0).unwrap();
    assert_eq!(store.purge_expired().unwrap(), vec!["reused"]);
    store.create("reused").unwrap();

    // Seed the replacement so a broken name-only check would visibly execute
    // the old keyed write instead of failing on a missing table.
    let (status, body) = call(
        &app,
        "/v1/databases/reused/query",
        &owner,
        json!({"sql":"CREATE TABLE t (value INTEGER)"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = call(
        &app,
        "/v1/databases/reused/query",
        &old_key,
        json!({"sql":"INSERT INTO t VALUES (1)"}),
        Some("replay-before-purge"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "database_forbidden");

    let (status, body) = call(
        &app,
        "/v1/databases/reused/query",
        &owner,
        json!({"sql":"SELECT count(*) FROM t"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([[0]]));

    let new_key = store
        .issue(KeyRequest {
            name: "new_app".into(),
            role: "database_read_write".into(),
            databases: vec!["reused".into()],
        })
        .unwrap();
    let (status, body) = call(
        &app,
        "/v1/databases/reused/query",
        &new_key,
        json!({"sql":"SELECT count(*) FROM t"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([[0]]));
}
