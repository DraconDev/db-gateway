use crate::{
    error::{Error, Result},
    execution::{Batch, Statement},
    store::{
        ACQUIRE_TIMEOUT, KeyRequest, Principal, QueryShape, StatementRun, Store, unauthorized,
        validate_name,
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Path, Query, State, rejection::JsonRejection},
    http::HeaderMap,
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use base64::Engine as _;
const BASE64_STANDARD: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Owner tombstone retention before physical file purge (24h).
pub const RETENTION_SECS: i64 = 24 * 3600;
/// Bounded inline export/import payload (over HTTP).
const MAX_EXPORT: usize = 8 * 1024 * 1024;
/// Chunked-transfer envelope: one 1 MiB chunk base64-encoded plus JSON.
const MAX_CHUNK_BODY: usize = 2 * 1024 * 1024;

/// Where this gateway is running.
///
/// The facts a client cannot work out for itself. A gateway is often
/// reached through a tunnel, so the client sees a loopback port and
/// nothing about the machine on the other end -- and that machine is the
/// one holding the data. The gateway is therefore the only place that can
/// answer "which host is this" and "which directory is it using".
#[derive(Clone, Debug)]
pub struct Deployment {
    /// The address the listener actually bound, which is not always the
    /// one that was asked for: `--listen 127.0.0.1:0` binds an ephemeral
    /// port, and a report of the requested address would be a lie.
    pub listen: SocketAddr,
    /// The machine's hostname, when the platform reports one.
    pub host: Option<String>,
}

pub fn router(store: Arc<Store>, deployment: Deployment) -> Router {
    Router::new()
        .route("/v1/databases", get(list).post(create))
        .route("/v1/contents", get(contents))
        .route("/v1/databases/{name}", get(info).delete(remove))
        // Undo half of delete: clears a tombstone inside its retention
        // window. The file was never unlinked, so this is a registry
        // change and nothing is copied or restored.
        .route("/v1/databases/{name}/undelete", post(undelete_route))
        // The delete-protection guard, set and cleared by the operator.
        .route(
            "/v1/databases/{name}/protection",
            put(set_protection).delete(clear_protection),
        )
        // The traffic blocks, set and cleared by the operator. Writes
        // and reads block independently: a cutover freezes writers
        // while every reader keeps working.
        .route(
            "/v1/databases/{name}/block-writes",
            put(set_writes_block).delete(clear_writes_block),
        )
        .route(
            "/v1/databases/{name}/block-reads",
            put(set_reads_block).delete(clear_reads_block),
        )
        .route("/v1/databases/{name}/schema", get(table_schema))
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
        // Bulk revoke by database scope. Registered under a distinct
        // path from /v1/keys/{name} so a database named "owner" can
        // never be mistaken for the owner key's revoke route.
        .route("/v1/database-keys/{name}", delete(revoke_database))
        .route("/v1/status", get(status))
        .route("/v1/backups", get(backups))
        .route("/v1/queries", get(queries))
        .route("/v1/databases/{name}/query", post(query))
        .route("/v1/databases/{name}/batch", post(batch))
        // The placement of this gateway is a property of the process, not
        // of the store, so it travels as an extension rather than inside
        // the shared state every handler already takes.
        .layer(Extension(deployment))
        .with_state(store.clone())
        // The comparison is its own router: it opens the two database
        // files themselves, read-only, and it is a read in every sense that
        // matters to the admission lanes.
        .merge(crate::diff::router(store.clone()))
        // The audit log is its own router too, and so are the three
        // database operations that need to open a file themselves: the
        // statement path cannot run a PRAGMA, and it cannot run a VACUUM
        // inside its transaction.
        .merge(crate::audit::router(store.clone()))
        .merge(crate::db_ops::router(store.clone()))
        // A backup set on demand: the gateway can take one now, in the
        // same shape the host's hourly producer writes, and it takes the
        // producer's lock so the two never publish at once.
        .merge(crate::backup_run::router(store.clone()))
        // The branch registry is read when the router is built, which is
        // startup: a fork made yesterday is still a fork today.
        .merge(crate::branch::router(store))
        // The general body cap goes LAST, after every merge.
        //
        // `Router::layer` maps over the routes that exist at the moment it
        // is called, and `Router::merge` inserts another router's routes
        // afterwards WITHOUT re-applying the outer router's layers. A cap
        // added before the merges therefore covered only the routes
        // declared above it: `/v1/diff` and `/v1/branch` fell back to
        // axum's own 2 MiB default and accepted a body 32x the documented
        // 64 KiB bound. Applying it here covers every route in the
        // gateway, merged or not, and the per-route caps declared with
        // `MethodRouter::layer` (the import's 12 MiB, the chunk body's
        // 2 MiB) still win on their own routes because a route-level layer
        // is applied closer to the handler.
        .layer(DefaultBodyLimit::max(65536))
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
///
/// The rule itself lives in [`crate::backup_health`], which the notification
/// delivery (observability contract §3 alert 4) also calls; re-exported here
/// because this is where operators and existing callers already look for it.
pub use crate::backup_health::STALE_AFTER_SECS;

/// Owner-only service status: version, uptime, admission config, and
/// request-pressure counters. The machine-readable counterpart to the
/// human `dg status`; AI operators poll this, not log files.
async fn status(
    State(store): State<Arc<Store>>,
    Extension(deployment): Extension<Deployment>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work_read(store, headers, move |s, p| {
        p.owner()?;
        let m = s.metrics.snapshot();
        let usage = s.metrics.usage_snapshot();
        let queue = s.metrics.queue_snapshot();
        let (live_databases, tombstoned_databases) = s.database_counts()?;
        let last_writes = s.last_writes();
        let mut databases: Vec<String> = usage.keys().chain(queue.keys()).cloned().collect();
        databases.sort();
        databases.dedup();
        let databases = databases
            .into_iter()
            .map(|name| {
                let rows = usage.get(&name).copied().unwrap_or_default();
                let waits = queue.get(&name).copied().unwrap_or_default();
                let last_write = last_writes
                    .get(&name)
                    .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                    .map(|at| at.as_secs());
                (
                    name,
                    json!({
                            "rows_read": rows.rows_read,
                            "rows_written": rows.rows_written,
                            "queue_waits": waits.waits,
                            "queue_wait_ms": waits.wait_ms,
                            "queue_wait_max_us": waits.max_wait_us,
                            // This database's OWN p95 of its own recent
                            // admission waits. The service-wide p95 lives
                            // under `requests` and covers every database,
                            // so a single contended database can hide in it.
                            "queue_wait_p95_us": waits.wait_p95_us,
                    "query_p95_us": waits.query_p95_us,
                            "queue_timeouts": waits.timeouts,
                            "last_write": last_write,
                        }),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        // Where this gateway is, and where each of its databases is. The
        // answer comes from the server because the client cannot know it:
        // a gateway reached through a tunnel looks like a loopback port
        // from the outside, and the machine on the other end is the one
        // holding the files.
        //
        // It costs a stat per database file, which is why the placement is
        // answered here rather than per database: one poll covers every
        // database, instead of the client asking once per name.
        let mut locations = serde_json::Map::new();
        let mut databases_bytes = 0u64;
        for location in s.locations()? {
            databases_bytes = databases_bytes.saturating_add(location.size_bytes);
            locations.insert(
                location.name.clone(),
                json!({
                    "file": location.file.to_string_lossy().into_owned(),
                    "exists": location.exists,
                    "size_bytes": location.size_bytes,
                    "tombstoned": location.tombstoned,
                }),
            );
        }
        let disk = crate::store::disk_space(s.root()).map(|space| {
            json!({
                "total_bytes": space.total_bytes,
                "free_bytes": space.free_bytes,
                "used_bytes": space.used_bytes,
                "used_percent": space.used_percent(),
                "total_inodes": space.total_inodes,
                "free_inodes": space.free_inodes,
            })
        });
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
                // The per-second counts of the recent window, oldest
                // first. A window, not a time series: enough to show
                // recent activity, bounded so it cannot grow.
                "recent_per_second": m.request_window,
                "recent_span_secs": m.window_span_secs,
                "wait_p50_us": m.wait_p50_us,
                "wait_p95_us": m.wait_p95_us,
                "wait_samples": m.wait_samples,
                "query_p50_us": m.query_p50_us,
                "query_p95_us": m.query_p95_us,
                "query_samples": m.query_samples,
            },
            "databases": databases,
            "database_counts": {
                "live": live_databases,
                "tombstoned": tombstoned_databases,
            },
            "deployment": {
                "host": deployment.host,
                "listen": deployment.listen.to_string(),
                "data_dir": s.root().display().to_string(),
                "backup_root": s.backup_root().map(|root| root.to_string_lossy().into_owned()),
                "databases": locations,
                // How much room is left on the filesystem the data lives
                // on. The observability contract names this as the
                // disk-pressure signal: low disk blocks safe writes,
                // imports and staging, and an operator wants to see it
                // before a write fails rather than after.
                "disk": disk,
                // The total the database files occupy. The directory
                // also holds the registry and staging, so this is the
                // databases' own size, not the directory's.
                "databases_bytes": databases_bytes,
                // How long a deleted name is held before it is purged:
                // a setting the operator should not have to remember.
                "retention_secs": RETENTION_SECS,
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
    work_read(store, headers, |s, p| {
        p.owner()?;
        let sets = s.backup_sets();
        let mut set_json = Vec::new();
        if let Some(ref sets) = sets {
            for set in sets {
                set_json.push(json!({
                    "timestamp": set.timestamp,
                    "databases": set.databases,
                    "has_registry": set.has_registry,
                    "coverage_start": set.coverage_start.and_then(|stamp| {
                        stamp.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
                    }),
                    "coverage_end": set.coverage_end.and_then(|stamp| {
                        stamp.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
                    }),
                }));
            }
        }
        // Freshness is computed by the shared rule, not re-derived here, so
        // this endpoint and the notification digest cannot disagree.
        let health = crate::backup_health::snapshot(s)?;
        let mut per_database = serde_json::Map::new();
        for db in &health.databases {
            per_database.insert(
                db.name.clone(),
                json!({
                    "state": db.state,
                    "newest_set": db.newest_set,
                    "age_secs": db.age_secs,
                    "lag_secs": db.lag_secs,
                }),
            );
        }
        // The last attempt is the only durable evidence of a run that
        // failed: a failed run publishes no set, so "no new set" cannot
        // tell a failure from a job that never ran.
        let attempt_json = |attempt: &crate::backup_health::AttemptRecord| {
            json!({
                "ts": attempt.ts,
                "started_at": attempt.started_at,
                "completed_at": attempt.completed_at,
                "outcome": attempt.outcome,
                "set": attempt.set,
                "databases": attempt.databases,
                "step": attempt.step,
            })
        };
        let last_attempt = health.last_attempt.as_ref().map(attempt_json);
        let last_failure = health.last_failure.as_ref().map(attempt_json);
        let coverage = health.coverage.as_ref().map(|coverage| {
            json!({
                "live": coverage.live,
                "in_newest_set": coverage.in_newest_set,
                "missing": coverage.missing,
                "registry": coverage.registry,
            })
        });
        let replica = health.replica.as_ref().map(|replica| {
            json!({
                "target": replica.target,
                "bucket": replica.bucket,
                "endpoint": replica.endpoint,
                "region": replica.region,
                "retention": replica.retention,
                "snapshot_interval": replica.snapshot_interval,
                "databases": replica.databases,
                "registry": replica.registry,
                "sidecar": replica.sidecar,
                "published_at": replica.published_at,
            })
        });
        let drill = health.drill.as_ref().map(|drill| {
            json!({
                "drilled_at": drill.drilled_at,
                "outcome": drill.outcome,
                "databases": drill.databases,
                "detail": drill.detail,
            })
        });
        // The window of moments the off-host replica can be restored to,
        // and the last restore that was taken from it. Both are the
        // host's to know and the host's to publish; the gateway reports
        // them the way it reports where the copy is.
        let restore_points = health.restore_points.as_ref().map(|points| {
            let databases: serde_json::Map<String, serde_json::Value> = points
                .databases
                .iter()
                .map(|(name, window)| {
                    (
                        name.clone(),
                        json!({
                            "oldest": window.oldest,
                            "newest": window.newest,
                            "files": window.files,
                            "bytes": window.bytes,
                        }),
                    )
                })
                .collect();
            json!({
                "published_at": points.published_at,
                "window_start": points.window_start,
                "window_end": points.window_end,
                "sidecar": points.sidecar,
                "databases": databases,
            })
        });
        let last_restore = health.last_restore.as_ref().map(|restore| {
            json!({
                "restored_at": restore.restored_at,
                "database": restore.database,
                "timestamp": restore.timestamp,
                "outcome": restore.outcome,
                "detail": restore.detail,
                "tables": restore.tables,
                "rows": restore.rows,
                "size_bytes": restore.size_bytes,
                "integrity": restore.integrity,
                "path": restore.path,
            })
        });
        Ok(Json(json!({
            "backup_root": s.backup_root().map(|p| p.to_string_lossy().into_owned()),
            "stale_after_secs": STALE_AFTER_SECS,
            // The owner-approved snapshot cadence the states were judged
            // against. `null` when the owner has not approved one, in which
            // case every unrepresented change is reported.
            "cadence_secs": health.cadence_secs,
            "sets": set_json,
            "per_database": per_database,
            "deleted": health.deleted,
            "last_attempt": last_attempt,
            "last_failure": last_failure,
            "coverage": coverage,
            "replica": replica,
            "drill": drill,
            "restore_points": restore_points,
            "last_restore": last_restore,
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
        crate::audit::record(s, &p, "create", Some(&body.name), None);
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
        let label = body.name.clone();
        let key = s.issue(body)?;
        // The credential's LABEL is recorded, never the key it just
        // issued: an audit log that held the key would be the leak it
        // exists to prevent.
        crate::audit::record(s, &p, "issue_key", None, Some(label));
        Ok(Json(json!({"key":key})))
    })
    .await
}
/// Owner-only credential inventory. Hashes are never serialized.
async fn list_keys(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work_read(store, headers, |s, p| {
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
        crate::audit::record(s, &p, "revoke_key", None, Some(name.clone()));
        Ok(Json(json!({"revoked":true})))
    })
    .await
}

/// Revoke every credential scoped to one database.
///
/// The bulk form of the credential revoke, for a leaked key: the operator
/// knows which database is exposed, not which credential names exist. It
/// never touches the owner key — that key reaches every database by
/// construction, so including it would revoke the credential the operator
/// is holding.
async fn revoke_database(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        let revoked = s.revoke_scoped_to(&name)?;
        crate::audit::record(
            s,
            &p,
            "revoke_database_keys",
            Some(&name),
            Some(json!({"revoked": revoked}).to_string()),
        );
        Ok(Json(json!({
            "database":name,
            "revoked":revoked,
            "owner_key_excluded":true,
        })))
    })
    .await
}

async fn rotate_owner(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work(store, headers, |s, p| {
        p.owner()?;
        let key = s.rotate_owner()?;
        crate::audit::record(s, &p, "rotate_owner", None, None);
        Ok(Json(json!({"key":key})))
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

/// How much data each live database holds: its table count and its
/// total row count, read from its file.
///
/// Owner-only, like the listing beside it. The answer exists because
/// the traffic counters cannot give it: they are in memory and reset
/// when the gateway restarts, so a database full of data reads as
/// nothing beside them. These counts come from the files.
///
/// The path is `/v1/contents` rather than `/v1/databases/contents`
/// because `contents` is a legal database name: a route under
/// `/v1/databases/` would be matched before `{name}`, so a database
/// called `contents` would have its own info and delete routes shadowed
/// and become unreachable. Outside `/v1/databases/`, no name can collide.
async fn contents(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work_read(store, headers, move |s, p| {
        p.owner()?;
        Ok(Json(s.contents()?))
    })
    .await
}

/// The structure of one database's tables: every column with its
/// declared type, its not-null and primary-key flags, and the table's
/// indexes and foreign keys.
///
/// Read-only and owner-only, like the listing beside it: the answer
/// says what is inside a database, and only the owner's credential
/// opens one. The structure is read from the database file with
/// SQLite's own PRAGMAs, because the query endpoint refuses PRAGMA --
/// a fact about the schema is not a statement to run.
///
/// `?table=<name>` narrows the answer to one table. The narrowing
/// happens on the server so that the document a client reads is the
/// API's own, narrowed or not.
async fn table_schema(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    query: std::result::Result<Query<SchemaFilter>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>> {
    let filter = query
        .map(|Query(filter)| filter)
        .map_err(|_| Error::invalid())?;
    work_read(store, headers, move |s, p| {
        p.owner()?;
        Ok(Json(s.schema(&name, filter.table.as_deref())?))
    })
    .await
}

/// The one filter the structure route takes.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaFilter {
    /// Only this table's structure.
    table: Option<String>,
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
        let _guard = s.acquire_queue(&name, true)?;
        // No existence pre-check here: delete's own 404/409 answers are the
        // contract, and the fenced write releases the per-name state again
        // when the request fails without a live row.
        s.fenced_write(&name, |s| s.delete(&name, RETENTION_SECS))?;
        crate::audit::record(s, &p, "delete", Some(&name), None);
        Ok(Json(
            json!({"deleted":true,"purge_after_seconds":RETENTION_SECS}),
        ))
    })
    .await
}

/// Bring a tombstoned database back inside its retention window.
async fn undelete_route(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        let _guard = s.acquire_queue(&name, true)?;
        s.fenced_write(&name, |s| s.undelete(&name))?;
        crate::audit::record(s, &p, "undelete", Some(&name), None);
        Ok(Json(json!({"undeleted":true})))
    })
    .await
}

/// Turn delete protection on for one database.
async fn set_protection(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.set_protected(&name, true)?;
        crate::audit::record(s, &p, "protect", Some(&name), None);
        Ok(Json(json!({"protected":true})))
    })
    .await
}

/// Turn delete protection off for one database.
async fn clear_protection(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.set_protected(&name, false)?;
        crate::audit::record(s, &p, "unprotect", Some(&name), None);
        Ok(Json(json!({"protected":false})))
    })
    .await
}

/// Block writes to one database. Owner-only like every other guard:
/// the switch that freezes traffic cannot itself be reachable from
/// the traffic it freezes.
async fn set_writes_block(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.set_writes_blocked(&name, true)?;
        crate::audit::record(s, &p, "block_writes", Some(&name), None);
        Ok(Json(json!({"writes_blocked":true})))
    })
    .await
}

/// Unblock writes to one database.
async fn clear_writes_block(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.set_writes_blocked(&name, false)?;
        crate::audit::record(s, &p, "unblock_writes", Some(&name), None);
        Ok(Json(json!({"writes_blocked":false})))
    })
    .await
}

/// Block reads from one database.
async fn set_reads_block(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.set_reads_blocked(&name, true)?;
        crate::audit::record(s, &p, "block_reads", Some(&name), None);
        Ok(Json(json!({"reads_blocked":true})))
    })
    .await
}

/// Unblock reads from one database.
async fn clear_reads_block(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        s.set_reads_blocked(&name, false)?;
        crate::audit::record(s, &p, "unblock_reads", Some(&name), None);
        Ok(Json(json!({"reads_blocked":false})))
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
        // The destination is a NEW database, so cloning is a creation
        // primitive: a scoped app key could name-squat a valid name and burn
        // slots out of the shared registry cap, exposing every other tenant.
        // Owner-only, matching create and import (owner decision 2026-10-10).
        p.owner()?;
        let _guard = s.acquire_queue(&body.destination, true)?;
        s.fenced_write(&body.destination, |s| {
            s.clone_database(&name, &body.destination)
        })?;
        crate::audit::record(s, &p, "clone", Some(&name), Some(body.destination.clone()));
        Ok(Json(json!({"source":name,"destination":body.destination})))
    })
    .await
}

/// Owner/authorized-app snapshot export. The snapshot is returned in the
/// response body (base64); the server never writes caller-named paths.
/// Streams are bounded by MAX_EXPORT.
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
        crate::audit::record(s, &p, "export", Some(&name), None);
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
        // The path segment is an informational source label (echoed back as
        // "from"); only the body's `name` chooses the new database. Validate
        // the body name up front and stage under it, so an invalid body name
        // fails before the base64 has been decoded and written to disk.
        let temp = s.import_staging(&body.name)?;
        std::fs::write(temp.path(), &bytes).map_err(|_| Error::internal())?;
        let _guard = s.acquire_queue(&body.name, true)?;
        s.fenced_write(&body.name, |s| s.import_from(&body.name, temp.path()))?;
        crate::audit::record(s, &p, "import", Some(&body.name), Some(name.clone()));
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
    // Validate the path segment the same way import_begin does. The code is
    // `invalid_request`, not `invalid_name`: `validate_name` returns
    // `Error::invalid()` and this project has no `invalid_name` code anywhere
    // (docs/api.md:69 documents `invalid_request` as the "bad name" code).
    // Naming a code that does not exist here would invite a future reader to
    // invent one and break the documented vocabulary.
    validate_name(&name)?;
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
    work_read(store, headers, move |s, p| {
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
        // Reject an oversized chunk before allocating the decoded buffer.
        // upload_append enforces MAX_CHUNK authoritatively, but by then the
        // base64 has already been decoded and a 2 MiB body has cost a ~1.5 MiB
        // allocation. Standard base64 carries 3 bytes per 4 characters, so
        // anything longer than 4*ceil(MAX_CHUNK/3) cannot decode small enough.
        if body.data_base64.len() > 4 * crate::store::MAX_CHUNK.div_ceil(3) {
            return Err(Error::new(
                413,
                "result_limit_exceeded",
                "chunk exceeds the per-chunk transfer ceiling",
            ));
        }
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
        let _guard = s.acquire_queue(&body.name, true)?;
        s.fenced_write(&body.name, |s| s.upload_commit(&id, &body.name))?;
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
        // The traffic blocks are checked after identity and existence
        // and before the queue: a tombstone still reports as a
        // tombstone, and a refused request never takes a queue slot.
        // The block binds every credential including the owner's — a
        // freeze with an exception is a freeze the cutover cannot
        // trust — and it binds only this path: inspect, schema,
        // export, clone, and backup stay owner-operated so the
        // operator can always see the flags and read the data out.
        if read {
            if s.reads_blocked(&name)?.unwrap_or(false) {
                return Err(Error::new(
                    403,
                    "reads_blocked",
                    "reads are blocked for this database",
                ));
            }
        } else if s.writes_blocked(&name)?.unwrap_or(false) {
            return Err(Error::new(
                403,
                "writes_blocked",
                "writes are blocked for this database",
            ));
        }
        let _guard = s.acquire_queue(&name, !read)?;
        let previous_write = if read { None } else { s.last_write(&name) };
        if !read {
            s.begin_write(&name)?;
        }
        let result = crate::execution::execute_detailed(
            &path,
            &statements,
            batch,
            p.role == "database_read_only",
            &p.hash,
            key.as_deref(),
        );
        if !read {
            // A successful replay committed nothing to the database: the
            // cached value came back from a previous request's transaction.
            // Closing the fence with finish_write would push the freshness
            // marker forward to now on every replay, muting the stale-backup
            // digest for a database whose real write predates the newest
            // backup. Restore the pre-request marker instead, the same as a
            // rolled-back mutation.
            let committed = result.as_ref().is_ok_and(|outcome| !outcome.replayed);
            if committed {
                s.finish_write(&name);
            } else {
                s.cancel_write(&name, previous_write);
            }
        }
        if let Ok(outcome) = result.as_ref() {
            let (rows_read, rows_written) = execution_usage(&outcome.value);
            s.metrics.record_usage(&name, rows_read, rows_written);
            record_statements(s, &name, &p, &statements, outcome, batch);
        }
        result.map(|outcome| Json(outcome.value))
    })
    .await
}

/// Sum rows read/written from a query/batch result: returned rows count as
/// read, rows_affected as written (RETURNING clauses legitimately count
/// both). Batch responses nest per-statement results under `results`.
fn execution_usage(value: &Value) -> (u64, u64) {
    fn one(result: &Value) -> (u64, u64) {
        let read = result["rows"]
            .as_array()
            .map(|rows| rows.len() as u64)
            .unwrap_or(0);
        let written = result["rows_affected"].as_u64().unwrap_or(0);
        (read, written)
    }
    match value["results"].as_array() {
        Some(results) => results
            .iter()
            .map(one)
            .fold((0, 0), |(reads, writes), (read, written)| {
                (reads + read, writes + written)
            }),
        None => one(value),
    }
}
/// The rows one statement's result carried.
fn statement_usage(result: &Value) -> (u64, u64) {
    let read = result["rows"]
        .as_array()
        .map(|rows| rows.len() as u64)
        .unwrap_or(0);
    let written = result["rows_affected"].as_u64().unwrap_or(0);
    (read, written)
}

/// Record what a request's statements cost, so the operator can ask
/// which one is the expensive one.
///
/// The shape is what is kept -- the SQL with its literals replaced -- so
/// the log answers "how much does this statement cost" without
/// becoming a second copy of the data.
fn record_statements(
    store: &Store,
    name: &str,
    principal: &Principal,
    statements: &[Statement],
    outcome: &crate::execution::ExecuteOutcome,
    batch: bool,
) {
    let usage: Vec<(u64, u64)> = match outcome.value["results"].as_array() {
        Some(results) => results.iter().map(statement_usage).collect(),
        None => vec![statement_usage(&outcome.value)],
    };
    let runs: Vec<StatementRun> = statements
        .iter()
        .enumerate()
        .map(|(index, statement)| {
            let (rows_read, rows_written) = usage.get(index).copied().unwrap_or((0, 0));
            StatementRun {
                sql: statement.sql.clone(),
                rows_read,
                rows_written,
                micros: outcome.statement_micros.get(index).copied().unwrap_or(0),
            }
        })
        .collect();
    store.metrics.queries.record(name, &principal.name, &runs);
    // The percentile rings take the same per-statement costs: the shape
    // log answers "which one is expensive", the rings answer "how slow
    // are we right now".
    for run in &runs {
        store.metrics.record_latency(name, run.micros);
    }
    // The audit log names the same statements, and names them the same
    // way: by their shape. A statement's values are the data, and the
    // data is not what the audit log is for.
    let action = if batch { "batch" } else { "query" };
    for run in &runs {
        crate::audit::record_statement(
            store,
            principal,
            action,
            Some(name),
            &run.sql,
            Some(run.micros),
        );
    }
}

/// One statement shape, as the API reports it.
fn shape_json(shape: &QueryShape) -> Value {
    let mut principals: Vec<(String, u64)> = shape
        .principals
        .iter()
        .map(|(name, count)| (name.clone(), *count))
        .collect();
    principals.sort();
    json!({
        "shape": shape.shape,
        "count": shape.count,
        "rows_read": shape.rows_read,
        "rows_written": shape.rows_written,
        "total_micros": shape.total_micros,
        "max_micros": shape.max_micros,
        "first_seen": shape.first_seen,
        "last_seen": shape.last_seen,
        "principals": principals.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
    })
}

/// The statement shapes the service has run, per database.
///
/// Owner-only: the log holds the SQL every credential ran, including
/// the keys an application holds, so it is the owner's view of their
/// own service and nobody else's.
/// The optional narrowing of the query log.
#[derive(Deserialize)]
struct QueriesQuery {
    /// Only this database's statements.
    database: Option<String>,
}

async fn queries(
    State(store): State<Arc<Store>>,
    query: std::result::Result<Query<QueriesQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let filter = query_params(query)?;
    work_read(store, headers, move |s, p| {
        p.owner()?;
        // The log is in memory and it outlives a deletion within the
        // life of the process, so it can still hold shapes for databases
        // that have been deleted. Those are history, not traffic: the tab
        // that shows them is answering "which statement is the expensive
        // one", and a database that is gone has no expensive statement.
        // They are counted rather than hidden, and the audit trail keeps
        // the record of what was run against them.
        let live = s.list_all()?;
        let mut deleted = 0usize;
        let databases: Vec<Value> = s
            .metrics
            .queries
            .all()
            .into_iter()
            .filter(|(database, _)| match &filter.database {
                Some(wanted) => database == wanted,
                None => true,
            })
            .filter(|(database, _)| {
                let alive = live.contains(database);
                if !alive {
                    deleted += 1;
                }
                alive
            })
            .map(|(database, shapes)| {
                json!({
                    "database": database,
                    "shapes": shapes.iter().map(shape_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(Json(json!({ "databases": databases, "deleted": deleted })))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WRITE_PERMITS;

    /// A request is shed with 429 when the global lane is full.
    ///
    /// This is the lane's own bound, and it is checked here rather than
    /// over HTTP: filling the lane needs as many simultaneous slow
    /// requests as the lane has permits, and whether they overlap is
    /// something the machine decides — a test that spawns them and hopes
    /// is a test that fails on a busy box for a reason that has nothing
    /// to do with admission. Holding the permits directly makes the
    /// contention exact.
    #[tokio::test]
    async fn a_request_is_shed_when_the_lane_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let owner = store.bootstrap().unwrap();
        // Every write permit is taken, so the next write cannot be
        // admitted and waits out the bound.
        let mut held = Vec::new();
        for _ in 0..WRITE_PERMITS {
            held.push(store.write_lane.clone().acquire_owned().await.unwrap());
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {owner}").parse().unwrap(),
        );
        let error = work_in(store.clone(), headers, false, |_s, _p| -> Result<Value> {
            Ok(Value::Null)
        })
        .await
        .expect_err("a write must be shed when the lane is full");
        assert_eq!(error.status, 429);
        assert_eq!(error.code, "capacity_exceeded");
        drop(held);
    }
}
