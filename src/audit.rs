//! The audit endpoint, and the helper that fills it.
//!
//! The endpoint answers the question an operator asks after something
//! unexpected: which credential ran this, and when? A hosted provider
//! has the same log and cannot show it to the person who owns the box;
//! here the answer is the owner's.
//!
//! It is owner-only. The log holds what every credential on the box
//! did, so a scoped app key must not be able to read it: an
//! application that can read the audit log learns what the other
//! applications and the operator's own keys are doing, which is not
//! what a database-scoped key was issued for.
//!
//! What the endpoint returns is the log and nothing else: the
//! credential's label and role, the action, the database, and for a
//! statement its shape. The credential's secret is not in the log, so
//! it cannot be in the answer either.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::store::{ACQUIRE_TIMEOUT, Principal, Store, unauthorized};

/// The filters the endpoint takes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    /// Only the actions of this credential, named by its label.
    principal: Option<String>,
    /// Only what happened at or after this Unix second.
    since: Option<u64>,
    /// Keep only the newest entries.
    limit: Option<usize>,
}

/// The audit log, as the API serves it.
///
/// The router takes the store and hands back a router that is ready to
/// merge into the gateway's own: the same state, the same routes, one
/// more path.
pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/audit", get(entries))
        .with_state(store)
}

/// The recorded actions, newest last.
///
/// Owner-only: the log is the operator's view of every credential on
/// the box, so a scoped application key is refused it.
async fn entries(
    State(store): State<Arc<Store>>,
    headers: HeaderMap,
    query: std::result::Result<Query<Filters>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>> {
    let filters = query
        .map(|Query(filters)| filters)
        .map_err(|_| Error::invalid())?;
    let credential = credential(&headers)?;
    // The same admission the rest of the API uses: this is a read, so
    // it shares the wide read lane, and a full lane sheds it rather
    // than queueing it behind every writer.
    let metrics = store.clone();
    metrics.metrics.record_request();
    let lane = store.read_lane.clone();
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
        principal.owner()?;
        Ok::<_, Error>(store.audit.entries_matching(
            filters.principal.as_deref(),
            filters.since,
            filters.limit,
        ))
    })
    .await
    .map_err(|_| Error::internal())?;
    if let Err(error) = &result {
        metrics.metrics.record_error(error.code);
    }
    let entries = result?;
    Ok(Json(json!({ "entries": entries })))
}

/// The credential the request carries.
///
/// The API's own helper is private to that module, and the audit
/// endpoint is not the API module: it reads the same header, the same
/// way, so that a credential that works everywhere else works here.
fn credential(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(unauthorized)
}

/// Record an action in the audit log.
///
/// The action is the credential's label and role, what it did, and the
/// database it did it to. It does NOT record the credential's secret,
/// and it does not record the values of a statement: a caller with a
/// statement calls [`record_statement`], which keeps the statement's
/// shape and drops its literals.
///
/// It is cheap, and it never panics. The log is a bounded in-memory
/// ring, and a poisoned lock is recovered rather than propagated: an
/// audit entry is not worth taking the gateway down for.
pub fn record(
    store: &Store,
    principal: &Principal,
    action: &str,
    database: Option<&str>,
    detail: Option<String>,
) {
    store
        .audit
        .record_action(principal, action, database, detail);
}

/// Record a statement's action, keeping the statement's shape.
///
/// The detail is the SQL with every literal replaced by a placeholder:
/// the log says which statement ran, and never what the data was.
pub fn record_statement(
    store: &Store,
    principal: &Principal,
    action: &str,
    database: Option<&str>,
    sql: &str,
    elapsed_us: Option<u64>,
) {
    store
        .audit
        .record_statement(principal, action, database, sql, elapsed_us);
}
