use crate::{
    error::{Error, Result},
    execution::{Batch, Statement},
    store::{ACQUIRE_TIMEOUT, KeyRequest, Principal, Store, unauthorized, validate_name},
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
        .route(
            "/v1/snapshots/{id}",
            get(snapshot_chunk).delete(snapshot_release),
        )
        .route("/v1/uploads/{id}", get(upload_status).delete(upload_abort))
        .route(
            "/v1/uploads/{id}/chunk",
            post(upload_chunk).layer(DefaultBodyLimit::max(MAX_CHUNK_BODY)),
        )
        .route("/v1/uploads/{id}/commit", post(upload_commit))
        .route("/v1/keys", get(list_keys).post(issue))
        .route("/v1/keys/owner/rotate", post(rotate_owner))
        .route("/v1/keys/{name}", delete(revoke))
        .route("/v1/status", get(status))
        .route("/v1/backups", get(backups))
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
    work_in(store, headers, false, f).await
}
/// Read-lane variant of work(): SELECT-class traffic shares the wide read
/// lane instead of contending with writers for the narrow write lane.
/// Management mutations and statement-classified writes stay on work().
async fn work_read<T: Send + 'static>(
    store: Arc<Store>,
    headers: HeaderMap,
    f: impl FnOnce(&Store, Principal) -> Result<T> + Send + 'static,
) -> Result<T> {
    work_in(store, headers, true, f).await
}
async fn work_in<T: Send + 'static>(
    store: Arc<Store>,
    headers: HeaderMap,
    read: bool,
    f: impl FnOnce(&Store, Principal) -> Result<T> + Send + 'static,
) -> Result<T> {
    let credential = credential(&headers)?;
    // Bounded wait instead of instant shedding: bursty arrivals queue up to
    // ACQUIRE_TIMEOUT (tokio's semaphores are FIFO-fair, so waits don't
    // starve or reorder). Overload still surfaces as the same 429. Permit
    // remains owned by blocking work even when its HTTP caller disconnects.
    // The metrics clone exists so every exit below — including lane sheds —
    // is counted; nothing may `?`-return past the recording.
    let metrics = store.clone();
    metrics.metrics.record_request();
    let lane = if read {
        store.read_lane.clone()
    } else {
        store.write_lane.clone()
    };
    let permit = match tokio::time::timeout(ACQUIRE_TIMEOUT, lane.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        _ => {
            metrics.metrics.record_error("capacity_exceeded");
            return Err(Error::busy());
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _lease = store.request_lease()?;
        let principal = store.authenticate(&credential)?;
        f(&store, principal)
    })
    .await
    .map_err(|_| Error::internal())?;
    if let Err(e) = &result {
        metrics.metrics.record_error(e.code);
    }
    result
}
/// Staleness reference for `/v1/backups` (observability contract §2): committed
/// changes unrepresented by a verified copy for longer than this deserve an
/// operator look. An objective, not an accepted RPO.
pub const STALE_AFTER_SECS: u64 = 300;

/// Owner-only service status: version, uptime, admission config, and
/// request-pressure counters. The machine-readable counterpart to the
/// human `dg status`; AI operators poll this, not log files.
async fn status(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work(store, headers, |s, p| {
        p.owner()?;
        let m = s.metrics.snapshot();
        Ok(Json(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": m.uptime_secs,
            "admission": {
                "acquire_timeout_ms": crate::store::ACQUIRE_TIMEOUT.as_millis() as u64,
                "read_permits": crate::store::READ_PERMITS,
                "write_permits": crate::store::WRITE_PERMITS,
            },
            "requests": {
                "total": m.total,
                "capacity_exceeded": m.capacity_exceeded,
                "query_deadline_exceeded": m.deadline_exceeded,
                "internal_error": m.internal_error,
            },
        })))
    })
    .await
}

/// Owner-only backup freshness (observability contract §2, C2): versioned
/// sets newest-first plus per-database state derived from the last API
/// write vs the newest set containing the database. `unknown` — no root
/// configured, root unreadable, or no set contains the database — is a
/// reported state, never implied healthy. Freshness here is set membership
/// by timestamp, not a restore drill; restore readiness stays a separate
/// rehearsal (recovery_rehearsal).
async fn backups(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work(store, headers, |s, p| {
        p.owner()?;
        let live: Vec<String> = s.list(&p)?;
        let now = std::time::SystemTime::now();
        let sets = s.backup_sets();
        let mut set_json = Vec::new();
        if let Some(ref sets) = sets {
            for set in sets {
                set_json.push(json!({
                    "timestamp": set.timestamp,
                    "databases": set.databases,
                    "has_registry": set.has_registry,
                }));
            }
        }
        let mut per_database = serde_json::Map::new();
        let mut names: Vec<String> = live;
        if let Some(ref sets) = sets {
            for set in sets {
                for db in &set.databases {
                    if !names.contains(db) {
                        names.push(db.clone());
                    }
                }
            }
        }
        for name in &names {
            let newest = sets
                .as_ref()
                .and_then(|sets| sets.iter().find(|set| set.databases.contains(name)));
            let (state, newest_set, age_secs) = match newest {
                None => ("unknown", Value::Null, Value::Null),
                Some(set) => {
                    let age = now
                        .duration_since(set.mtime)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let state = match s.last_write(name) {
                        Some(wrote) if wrote > set.mtime => "stale",
                        Some(_) => "fresh",
                        // No durable API-write marker means the process cannot
                        // prove that the set represents all mutations (notably
                        // after restart or an older registry). Stay unknown,
                        // never falsely fresh.
                        None => "unknown",
                    };
                    (
                        state,
                        Value::String(set.timestamp.clone()),
                        Value::from(age),
                    )
                }
            };
            per_database.insert(
                name.clone(),
                json!({"state": state, "newest_set": newest_set, "age_secs": age_secs}),
            );
        }
        Ok(Json(json!({
            "backup_root": s.backup_root().map(|p| p.to_string_lossy().into_owned()),
            "stale_after_secs": STALE_AFTER_SECS,
            "sets": set_json,
            "per_database": per_database,
        })))
    })
    .await
}
async fn list(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    // Scoped listing: the owner sees every live database; an app key sees
    // exactly its authorized live databases (generation-bound, tombstones
    // excluded). Unknown/revoked credentials are still 401.
    work_read(store, headers, |s, p| {
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
        s.create_and_note(&body.name)?;
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

async fn rotate_owner(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work(store, headers, |s, p| {
        p.owner()?;
        Ok(Json(json!({"key":s.rotate_owner()?})))
    })
    .await
}

/// Owner-only database facts: size, tables, creation, tombstone state.
async fn info(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work_read(store, headers, move |s, p| {
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
        s.note_write(&name);
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
        s.note_write(&body.destination);
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
    work_read(store, headers, move |s, p| {
        s.authorize(&p, &name)?;
        let temp = s.export_snapshot_limited(&name, Some(MAX_EXPORT as u64))?;
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
        s.note_write(&body.name);
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
    work_read(store, headers, move |s, p| {
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
    params
        .map(|Query(value)| value)
        .map_err(|_| Error::invalid())
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
    work_read(store, headers, move |s, p| {
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
        s.note_write(&body.name);
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
    // Lane classification before admission: all-read-only statements share
    // the read lane and the shared per-database guard; anything else takes
    // the write lane and the exclusive guard. Misclassification degrades in
    // exactly one safe direction — a read on the write lane just waits
    // longer — because is_read_only fails closed and execution re-validates.
    let read = crate::execution::is_read_only(&statements);
    work_in(store, headers, read, move |s, p| {
        s.authorize(&p, &name)?;
        let path = s.path(&name)?;
        let _guard = s.acquire_queue(&name, !read)?;
        let result = crate::execution::execute(
            &path,
            &statements,
            batch,
            p.role == "database_read_only",
            &p.hash,
            key.as_deref(),
        );
        if !read && result.is_ok() {
            s.note_write(&name);
        }
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
