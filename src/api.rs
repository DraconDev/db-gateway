use crate::{
    error::{Error, Result},
    execution::{Batch, Statement},
    store::{KeyRequest, Principal, Store, unauthorized},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::HeaderMap,
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/databases", get(list).post(create))
        .route("/v1/keys", post(issue))
        .route("/v1/keys/{name}", delete(revoke))
        .route("/v1/databases/{name}/query", post(query))
        .route("/v1/databases/{name}/batch", post(batch))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(store)
}
fn json_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value).map_err(|err| {
        Error::new(
            err.status().as_u16(),
            "invalid_request",
            "malformed or oversized JSON body",
        )
    })
}
fn credential(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(unauthorized)
}
async fn work<T: Send + 'static>(
    store: Arc<Store>,
    headers: HeaderMap,
    f: impl FnOnce(&Store, Principal) -> Result<T> + Send + 'static,
) -> Result<T> {
    let credential = credential(&headers)?;
    // Reject rather than accumulate unbounded waiting tasks. Permit remains
    // owned by blocking work even when its HTTP caller disconnects.
    let permit = store
        .global
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::busy())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let principal = store.authenticate(&credential)?;
        f(&store, principal)
    })
    .await
    .map_err(|_| Error::internal())?
}
async fn list(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    // Listing is a management route: owner only, like create/issue/revoke.
    work(store, headers, |s, p| {
        p.owner()?;
        Ok(Json(json!({"databases":s.list(&p)?})))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
}
async fn create(
    State(store): State<Arc<Store>>,
    headers: HeaderMap,
    body: std::result::Result<Json<Create>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        p.owner()?;
        s.create(&body.name)?;
        Ok(Json(json!({"name":body.name})))
    })
    .await
}
async fn issue(
    State(store): State<Arc<Store>>,
    headers: HeaderMap,
    body: std::result::Result<Json<KeyRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        p.owner()?;
        Ok(Json(json!({"key":s.issue(body)?})))
    })
    .await
}
async fn revoke(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.revoke(&name)?;
        Ok(Json(json!({"revoked":true})))
    })
    .await
}
async fn execute(
    store: Arc<Store>,
    headers: HeaderMap,
    name: String,
    statements: Vec<Statement>,
    batch: bool,
) -> Result<Json<Value>> {
    let key = headers
        .get("idempotency-key")
        .map(|h| h.to_str().map(str::to_string).map_err(|_| Error::invalid()))
        .transpose()?;
    work(store, headers, move |s, p| {
        p.database(&name)?;
        let path = s.path(&name)?;
        let permit = s
            .queue(&name)?
            .try_acquire_owned()
            .map_err(|_| Error::busy())?;
        let result = crate::execution::execute(
            &path,
            &statements,
            batch,
            p.role == "database_read_only",
            &p.hash,
            key.as_deref(),
        );
        drop(permit);
        result.map(Json)
    })
    .await
}
async fn query(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Statement>, JsonRejection>,
) -> Result<Json<Value>> {
    execute(store, headers, name, vec![json_body(body)?], false).await
}
async fn batch(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Batch>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    if !body.atomic {
        return Err(Error::invalid());
    }
    execute(store, headers, name, body.statements, true).await
}
