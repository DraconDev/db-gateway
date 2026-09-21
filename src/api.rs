use crate::{
    error::{Error, Result},
    execution::{Batch, Statement},
    store::{KeyRequest, Principal, Store, unauthorized, validate_name},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
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
/// Chunked-transfer envelope: one 1 MiB chunk base64-encoded plus JSON.
const MAX_CHUNK_BODY: usize = 2 * 1024 * 1024;

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/databases", get(list).post(create))
        .route("/v1/databases/{name}", get(info).delete(remove))
        .route("/v1/databases/{name}/clone", post(clone_route))
        .route("/v1/databases/{name}/export", post(export))
        .route(
            "/v1/databases/{name}/import",
            post(import).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        .route("/v1/databases/{name}/export/begin", post(export_begin))
        .route("/v1/databases/{name}/import/begin", post(import_begin))
        .route("/v1/snapshots/{id}", get(snapshot_chunk).delete(snapshot_release))
        .route("/v1/uploads/{id}", get(upload_status).delete(upload_abort))
        .route(
            "/v1/uploads/{id}/chunk",
            post(upload_chunk).layer(DefaultBodyLimit::max(MAX_CHUNK_BODY)),
        )
        .route("/v1/uploads/{id}/commit", post(upload_commit))
        .route("/v1/keys", get(list_keys).post(issue))
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
/// Owner-only credential inventory. Hashes are never serialized.
async fn list_keys(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work(store, headers, |s, p| {
        p.owner()?;
        Ok(Json(s.list_keys()?))
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

/// Owner-only database facts: size, tables, creation, tombstone state.
async fn info(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        Ok(Json(s.info(&name)?))
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
        s.authorize(&p, &name)?;
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
        s.authorize(&p, &name)?;
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
        // Import publishes a new database and is an owner-only management operation.
        p.owner()?;
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
/// Chunked download: freeze a consistent snapshot, then fetch it in bounded
/// pieces. Same authorization as single-shot export.
async fn export_begin(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ExportRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    json_body(body)?;
    work(store, headers, move |s, p| {
        s.authorize(&p, &name)?;
        let info = s.export_begin(&p, &name)?;
        Ok(Json(
            json!({"snapshot":info.id,"size":info.size,"sha256":info.sha256}),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct ChunkQuery {
    offset: Option<u64>,
    length: Option<usize>,
}

fn query_params<T>(
    params: std::result::Result<Query<T>, axum::extract::rejection::QueryRejection>,
) -> Result<T> {
    params.map(|Query(value)| value).map_err(|_| Error::invalid())
}

async fn snapshot_chunk(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    query: std::result::Result<Query<ChunkQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let query = query_params(query)?;
    let offset = query.offset.unwrap_or(0);
    let length = query.length.unwrap_or(crate::store::MAX_CHUNK);
    work(store, headers, move |s, p| {
        let (bytes, size, sha256) = s.snapshot_chunk(&p, &id, offset, length)?;
        Ok(Json(json!({"snapshot":id,"size":size,"sha256":sha256,"offset":offset,"data_base64":BASE64_STANDARD.encode(bytes)})))
    })
    .await
}

async fn snapshot_release(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        s.snapshot_release(&p, &id)?;
        Ok(Json(json!({"released": true})))
    })
    .await
}

/// Chunked upload: open an upload, append chunks at the exact received
/// offset, then commit under a target name. Owner-only like import.
async fn import_begin(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ExportRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    json_body(body)?;
    work(store, headers, move |s, p| {
        p.owner()?;
        // The path segment is an informational label, as on import.
        validate_name(&name)?;
        let info = s.upload_begin()?;
        Ok(Json(json!({"upload": info.id})))
    })
    .await
}

async fn upload_status(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        let received = s.upload_status(&id)?;
        Ok(Json(
            json!({"upload":id,"received":received,"size_cap":crate::store::MAX_TRANSFER_SIZE}),
        ))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadChunk {
    offset: u64,
    data_base64: String,
}

async fn upload_chunk(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<UploadChunk>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        p.owner()?;
        let bytes = BASE64_STANDARD
            .decode(body.data_base64.as_bytes())
            .map_err(|_| Error::invalid())?;
        let received = s.upload_append(&id, body.offset, &bytes)?;
        Ok(Json(json!({"upload":id,"received":received})))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitRequest {
    name: String,
}

async fn upload_commit(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<CommitRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        p.owner()?;
        s.upload_commit(&id, &body.name)?;
        Ok(Json(json!({"imported": body.name})))
    })
    .await
}

async fn upload_abort(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.upload_abort(&id)?;
        Ok(Json(json!({"aborted": true})))
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
        s.authorize(&p, &name)?;
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
