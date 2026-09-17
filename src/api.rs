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
use sha2::{Digest, Sha256};
use std::sync::Arc;

use base64::Engine as _;
const BASE64_STANDARD: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Owner tombstone retention before physical file purge (24h).
const RETENTION_SECS: i64 = 24 * 3600;
/// Bounded inline export/import payload (over HTTP).
const MAX_EXPORT: usize = 8 * 1024 * 1024;

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/databases", get(list).post(create))
        .route("/v1/databases/{name}", delete(remove))
        .route("/v1/databases/{name}/clone", post(clone_route))
        .route("/v1/databases/{name}/export", post(export))
        .route(
            "/v1/databases/{name}/import",
            post(import).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        .route("/v1/keys", post(issue))
        .route("/v1/keys/{name}", delete(revoke))
        .route("/v1/databases/{name}/query", post(query))
        .route("/v1/databases/{name}/batch", post(batch))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(store)
}
fn json_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value).map_err(|err| {
        // Schema errors are bad requests in our public API, not Axum's 422.
        // Preserve other categories (notably 413 size and 415 content type).
        let status = match &err {
            JsonRejection::JsonDataError(_) => 400,
            _ => err.status().as_u16(),
        };
        Error::new(
            status,
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
        let _lease = store.request_lease()?;
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

/// Owner-only tombstone deletion. The SQLite file is purged later
/// (RETENTION_SECS); until then the name is neither queryable nor recreatable.
async fn remove(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.delete(&name, RETENTION_SECS)?;
        Ok(Json(
            json!({"deleted":true,"purge_after_seconds":RETENTION_SECS}),
        ))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloneRequest {
    destination: String,
}
async fn clone_route(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<CloneRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        // Cloning requires write access to the source database.
        p.database(&name)?;
        if p.role == "database_read_only" {
            return Err(Error::new(403, "forbidden", "read-only credential"));
        }
        s.clone_database(&name, &body.destination)?;
        Ok(Json(json!({"source":name,"destination":body.destination})))
    })
    .await
}

/// Owner/authorized-app snapshot export. The body may name an output file
/// under the server data dir; without "file" the snapshot is returned in the
/// response body (base64). Streams are bounded by MAX_EXPORT.
async fn export(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ExportRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    json_body(body)?;
    work(store, headers, move |s, p| {
        p.database(&name)?;
        let temp = s.export_snapshot(&name)?;
        if std::fs::metadata(temp.path()).map_err(|_| Error::internal())?.len() > MAX_EXPORT as u64 {
            return Err(Error::new(413, "result_limit_exceeded", "export exceeds bounded response size"));
        }
        let bytes = std::fs::read(temp.path()).map_err(|_| Error::internal())?;
        Ok(Json(json!({"database":name,"sha256":hex::encode(Sha256::digest(&bytes)),"size":bytes.len(),"data_base64":BASE64_STANDARD.encode(bytes)})))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportRequest {
    name: String,
    data_base64: String,
}
async fn import(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ImportRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        // Import into the database referenced by the path name.
        p.database(&name)?;
        if p.role == "database_read_only" {
            return Err(Error::new(403, "forbidden", "read-only credential"));
        }
        let bytes = BASE64_STANDARD
            .decode(body.data_base64.as_bytes())
            .map_err(|_| Error::invalid())?;
        if bytes.len() > MAX_EXPORT {
            return Err(Error::new(
                413,
                "result_limit_exceeded",
                "import exceeds bounded request size",
            ));
        }
        let temp = s.import_staging(&name)?;
        std::fs::write(temp.path(), &bytes).map_err(|_| Error::internal())?;
        s.import_from(&body.name, temp.path())?;
        Ok(Json(json!({"imported":body.name,"from":name})))
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
