//! The database operations that are questions about a database rather
//! than changes to it: what SQLite would do with a statement, whether
//! the file is sound, how much space the file is wasting.
//!
//! These are the cheap commands an operator reaches for instead of
//! writing a script to ask the same thing. None of them is a fake: each
//! one runs the real SQLite operation and reports what SQLite said.
//!
//! Two of them cannot travel the ordinary statement path. That path
//! installs an authorizer whose catch-all refuses PRAGMA, and it wraps
//! statements in a transaction, which SQLite refuses to VACUUM inside.
//! So the integrity check and the vacuum open the database file
//! themselves, outside both, and they take the write lane and the
//! freshness fence when they change anything.

use crate::error::{Error, Result};
use crate::store::{ACQUIRE_TIMEOUT, Principal, Store, unauthorized};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::HeaderMap,
    routing::{get, post},
};
use rusqlite::{Connection, OpenFlags, limits::Limit};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path as FilePath;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many problems a report lists.
///
/// The check itself always runs to completion -- a report that stopped
/// early could not say whether the file is sound -- but a file with ten
/// thousand broken rows would produce a response no caller can read.
/// The count of what was found is always reported, so a truncated list
/// is never mistaken for a short one.
const MAX_REPORTED: usize = 100;

/// How long a check may run before it is called off.
///
/// A full integrity check reads every page of the file, so it is much
/// slower than a statement on a large database. The budget exists so a
/// check cannot hold the lane for ever; a check that runs out of budget
/// reports that it did not finish rather than claiming the file is
/// sound.
const CHECK_BUDGET: Duration = Duration::from_secs(30);

/// The routes the operator's questions about a database travel on.
///
/// The integrator merges this into the API router: the endpoints need
/// the store, and the store is what the API router already holds.
pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/databases/{name}/integrity", get(integrity))
        .route("/v1/databases/{name}/vacuum", post(vacuum))
        .route("/v1/databases/{name}/explain", post(explain))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(store)
}

/// The credential a request carries.
///
/// `api.rs` has the same helper, but it is private to that module and
/// this file may not change it, so the rule is repeated here rather
/// than reinvented: the credential is a bearer token and nothing else
/// counts as one.
fn credential(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(unauthorized)
}

/// The admission every request in this server gets: a lane permit, a
/// lifecycle lease, then the credential.
///
/// `api.rs` runs the same sequence, and it is private to that module,
/// so it is repeated here: a request that skipped the lane would be a
/// request the server could not shed under load, and one that skipped
/// the lease could outlive the purge that unlinks the file underneath
/// it.
async fn work<T: Send + 'static>(
    store: Arc<Store>,
    headers: HeaderMap,
    read: bool,
    f: impl FnOnce(&Store, Principal) -> Result<T> + Send + 'static,
) -> Result<T> {
    let credential = credential(&headers)?;
    // The metrics clone exists so every exit below is counted; nothing
    // may return past the recording.
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

/// A JSON body that this API understands.
fn json_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value).map_err(|_| Error::invalid())
}

/// `GET /v1/databases/{name}/integrity`: is the file sound.
///
/// Owner-only: the check reads the whole file, and what the file
/// contains is the operator's business rather than an application's.
async fn integrity(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, true, move |s, p| {
        p.owner()?;
        let path = s.path(&name)?;
        // The shared guard keeps the check away from a writer. A vacuum
        // rewrites the file underneath it, and a check that read half
        // of a rewritten file would report damage that is not there.
        let _guard = s.acquire_queue(&name, false)?;
        let report = check_integrity(&name, &path)?;
        Ok(Json(report))
    })
    .await
}

/// `POST /v1/databases/{name}/vacuum`: reclaim the space the file is
/// wasting.
///
/// Owner-only and write-only: a vacuum rewrites the whole file, so it
/// is not something a read-only credential may ask for, and not
/// something an application's credential may ask for at all.
async fn vacuum(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, false, move |s, p| {
        s.authorize(&p, &name)?;
        if p.role == "database_read_only" {
            return Err(Error::new(403, "forbidden", "read-only credential"));
        }
        p.owner()?;
        let path = s.path(&name)?;
        // The exclusive per-database guard and the freshness fence are
        // the pair every other writer takes. A vacuum that ran beside
        // another writer would rewrite the file under it, and that is
        // corruption rather than a slow request.
        let _guard = s.acquire_queue(&name, true)?;
        let mut outcome = None;
        s.fenced_write(&name, |_| {
            outcome = Some(vacuum_file(&path)?);
            Ok(())
        })?;
        let outcome = outcome.ok_or_else(Error::internal)?;
        Ok(Json(json!({
            "database": name,
            "size_before_bytes": outcome.size_before_bytes,
            "size_after_bytes": outcome.size_after_bytes,
            "reclaimed_bytes": outcome.reclaimed_bytes,
            "checkpoint_complete": outcome.checkpoint_complete,
            "duration_ms": outcome.duration_ms,
        })))
    })
    .await
}

/// The statement whose plan is wanted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplainRequest {
    sql: String,
}

/// `POST /v1/databases/{name}/explain`: what SQLite would do with a
/// statement.
///
/// The ordinary query path can run `EXPLAIN QUERY PLAN`, but it
/// answers a statement that does not compile with a fixed sentence
/// ("SQL or parameters invalid"), and "no such table: t" is the
/// diagnosis. So the plan is asked for here, on a connection that
/// cannot write, and SQLite's own words are kept.
async fn explain(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ExplainRequest>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, true, move |s, p| {
        // A read-only credential may plan a read: it is a question, and
        // the answer changes nothing.
        s.authorize(&p, &name)?;
        let path = s.path(&name)?;
        let _guard = s.acquire_queue(&name, false)?;
        let plan = explain_plan(&path, &body.sql, &p)?;
        Ok(Json(json!({
            "database": name,
            "sql": body.sql,
            "plan": plan,
        })))
    })
    .await
}

/// One row of `PRAGMA foreign_key_check`.
#[derive(Serialize)]
struct ForeignKeyViolation {
    table: String,
    /// The row that breaks the constraint. A mismatch rather than a
    /// broken row reports no rowid, so this is optional.
    rowid: Option<i64>,
    parent: String,
    fkid: i64,
}

/// What SQLite said about the file.
fn check_integrity(name: &str, path: &FilePath) -> Result<Value> {
    let started = Instant::now();
    // Read-only: a diagnosis must not change the file it diagnoses.
    let conn = open_read_only(path)?;
    let deadline = Instant::now() + CHECK_BUDGET;
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;

    // `integrity_check` is the check that finds a corrupt page. It
    // answers with one row per problem, and the words in those rows
    // are the diagnosis, so they are kept as they come.
    let mut integrity = Vec::new();
    let mut integrity_total = 0usize;
    let mut failure: Option<String> = None;
    {
        let mut statement = conn.prepare("PRAGMA integrity_check")?;
        let mut rows = statement.query([])?;
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    integrity_total += 1;
                    if integrity.len() < MAX_REPORTED {
                        integrity.push(row.get::<_, String>(0)?);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    failure = Some(failure_text(&error));
                    break;
                }
            }
        }
    }

    // `foreign_key_check` is a separate question: the file can be
    // perfectly sound and still hold rows that point at rows that are
    // not there.
    let mut foreign_keys = Vec::new();
    let mut foreign_key_count = 0usize;
    {
        let mut statement = conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = statement.query([])?;
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    foreign_key_count += 1;
                    if foreign_keys.len() < MAX_REPORTED {
                        foreign_keys.push(ForeignKeyViolation {
                            table: row.get(0)?,
                            rowid: row.get(1)?,
                            parent: row.get(2)?,
                            fkid: row.get(3)?,
                        });
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    failure = failure.or_else(|| Some(failure_text(&error)));
                    break;
                }
            }
        }
    }

    // "ok" means both checks ran to completion and found nothing. A
    // check that was called off has not cleared the file, so it cannot
    // say it is sound.
    let complete = failure.is_none();
    let sound = complete
        && integrity_total == 1
        && integrity
            .first()
            .is_some_and(|m| m.eq_ignore_ascii_case("ok"))
        && foreign_key_count == 0;
    Ok(json!({
        "database": name,
        "ok": sound,
        "complete": complete,
        "integrity": integrity,
        "foreign_keys": foreign_keys,
        "foreign_key_count": foreign_key_count,
        "truncated": integrity_total > integrity.len() || foreign_key_count > foreign_keys.len(),
        "duration_ms": started.elapsed().as_millis() as u64,
        // Not "error": that key means the request itself failed, and a
        // file that is not sound is an answer, not a refused question.
        "failure": failure,
    }))
}

/// Why a check stopped early.
///
/// SQLite's own words are kept, because they name the problem; only
/// the interruption is described in this project's words, because
/// "interrupted" says nothing about what happened.
fn failure_text(error: &rusqlite::Error) -> String {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
        format!("the check did not finish within its {CHECK_BUDGET:?} budget")
    } else {
        error.to_string()
    }
}

/// What a vacuum reclaimed.
struct VacuumOutcome {
    size_before_bytes: u64,
    size_after_bytes: u64,
    reclaimed_bytes: u64,
    checkpoint_complete: bool,
    duration_ms: u64,
}

/// Reclaim the space the file is wasting.
fn vacuum_file(path: &FilePath) -> Result<VacuumOutcome> {
    let started = Instant::now();
    let size_before_bytes = file_size(path);
    let conn = open_read_write(path)?;
    // VACUUM cannot run inside a transaction, and this connection is in
    // autocommit: the statement is a transaction of its own.
    conn.execute_batch("VACUUM").map_err(sqlite_failure)?;
    // In WAL mode the rewritten database lands in the write-ahead log,
    // so the file on disk is still the old one until that log is folded
    // back in. Without this the file would keep the space it was
    // wasting and the report would claim nothing was reclaimed.
    let (busy, _log, _checkpointed) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(sqlite_failure)?;
    let size_after_bytes = file_size(path);
    Ok(VacuumOutcome {
        size_before_bytes,
        size_after_bytes,
        // A vacuum that leaves the file larger has reclaimed nothing;
        // the number is never dressed up as a saving.
        reclaimed_bytes: size_before_bytes.saturating_sub(size_after_bytes),
        checkpoint_complete: busy == 0,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// The size of the file, or zero when it cannot be read.
fn file_size(path: &FilePath) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Open a database for reading, and for nothing else.
///
/// The connection cannot write, so nothing that runs through it can
/// change the file: that is what makes the integrity check a diagnosis
/// and not a repair.
fn open_read_only(path: &FilePath) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(250))?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    Ok(conn)
}

/// Open a database for writing.
fn open_read_write(path: &FilePath) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(250))?;
    // The same connection settings the ordinary statement path uses, so
    // a vacuum writes under the same rules as the statements it
    // follows.
    conn.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;",
    )?;
    // The attachment limit the statement path sets is left off here:
    // VACUUM builds the new file in an attached database of its own, and
    // the limit would refuse the very statement it is here to run. No
    // caller's SQL runs on this connection.
    Ok(conn)
}

/// The plan SQLite gives for a statement.
///
/// The statement is prepared on its own first: that is where SQLite
/// says a statement does not compile, and the message it says is the
/// diagnosis the caller needs.
fn explain_plan(path: &FilePath, sql: &str, principal: &Principal) -> Result<Value> {
    let trimmed = sql.trim();
    if trimmed.is_empty() || sql.len() > 64 * 1024 {
        return Err(Error::invalid());
    }
    let conn = open_read_only(path)?;
    let statement = conn.prepare(trimmed).map_err(sqlite_failure)?;
    // A read-only credential may not plan a statement that writes, even
    // though the plan is never run: the answer would tell the caller
    // what a write it is not allowed to make would do.
    if principal.role == "database_read_only" && !statement.readonly() {
        return Err(Error::new(403, "sql_forbidden", "read-only credential"));
    }
    drop(statement);

    // The plan is asked for as "EXPLAIN QUERY PLAN", unless the caller
    // already wrote it that way.
    let planned = if trimmed
        .get(..18)
        .is_some_and(|head| head.eq_ignore_ascii_case("EXPLAIN QUERY PLAN"))
    {
        trimmed.to_string()
    } else {
        format!("EXPLAIN QUERY PLAN {trimmed}")
    };
    let mut statement = conn.prepare(&planned).map_err(sqlite_failure)?;
    let (id_index, parent_index, detail_index) = {
        let names = statement.column_names();
        (
            names.iter().position(|n| *n == "id").unwrap_or(0),
            names.iter().position(|n| *n == "parent").unwrap_or(1),
            names
                .iter()
                .position(|n| *n == "detail")
                .unwrap_or_else(|| names.len().saturating_sub(1)),
        )
    };
    let mut plan = Vec::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let detail: String = row.get(detail_index)?;
        plan.push(json!({
            "id": row.get::<_, i64>(id_index)?,
            "parent": row.get::<_, i64>(parent_index)?,
            "detail": detail,
            "meaning": meaning(&detail),
        }));
    }
    Ok(Value::Array(plan))
}

/// SQLite's message, kept.
///
/// The rest of the gateway maps a SQLite failure to a fixed sentence.
/// That is right for a statement whose failure the caller can
/// reproduce, and wrong here: "no such table: t" is the diagnosis, and
/// a fixed sentence throws it away. The status and the code stay the
/// ones the gateway uses everywhere else.
fn sqlite_failure(error: rusqlite::Error) -> Error {
    let message = error.to_string();
    let mapped = Error::from(error);
    Error::with_message(mapped.status, mapped.code, message)
}

/// What a plan line means.
///
/// SQLite's query plan carries no costs, so nothing here invents one:
/// each phrase says what the step does, not what it costs. A step this
/// does not recognise gets no phrase at all, rather than a wrong one.
fn meaning(detail: &str) -> &'static str {
    let detail = detail.trim_start();
    if detail.starts_with("SCAN CONSTANT ROW") {
        "reads no table: the row is a constant"
    } else if detail.starts_with("SCAN ") {
        "full scan: every row of the table is read"
    } else if detail.starts_with("SEARCH ") {
        if detail.contains("USING INTEGER PRIMARY KEY") {
            "primary key lookup: only the matching row is read"
        } else {
            "index lookup: only the matching rows are read"
        }
    } else if detail.starts_with("USE TEMP B-TREE FOR ORDER BY") {
        "sorts the rows through a temporary b-tree"
    } else if detail.starts_with("USE TEMP B-TREE FOR GROUP BY") {
        "groups the rows through a temporary b-tree"
    } else if detail.starts_with("CORRELATED ") {
        "runs the subquery once per row"
    } else if detail.starts_with("LIST SUBQUERY") || detail.starts_with("SCAN SUBQUERY") {
        "runs the subquery once"
    } else if detail.starts_with("BLOOM FILTER ON") {
        "skips rows with a bloom filter before the lookup"
    } else if detail.starts_with("MULTI-INDEX OR") {
        "uses more than one index"
    } else {
        ""
    }
}
