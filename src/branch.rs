//! Forks of a database: a copy that can diverge from what it came from.
//!
//! A branch is the thing a hosted provider will not give an operator:
//! a second copy of a database that starts at a known moment and then
//! goes its own way. Here the copy is made inside the gateway, from the
//! same files the live database uses, so the fork is the data and not
//! a description of it.
//!
//! What a branch is NOT is a backup. A branch is writable and it is
//! live: it diverges the moment it exists. The backup chain is what
//! keeps the past; a branch is what lets the future fork.
//!
//! Two sources are possible, and they answer different questions:
//!
//! * the LIVE parent, which is the database as it is right now. This is
//!   the same copy `dg clone` makes -- the SQLite backup primitive --
//!   so a fork and a clone are the same bytes.
//! * a MOMENT in the past, taken from a published backup set. The set
//!   is verified before anything is copied out of it: a fork of a set
//!   that does not verify would hand the operator a database whose
//!   contents nobody can account for.
//!
//! A branch of a branch is a branch. The parent may itself be a fork,
//! and the record says the immediate parent, so the chain of origin is
//! readable one link at a time.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::HeaderMap,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::store::{ACQUIRE_TIMEOUT, BranchRecord, Principal, Store, unauthorized};

/// A request to fork a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkRequest {
    /// The database to fork.
    pub parent: String,
    /// The name the fork is published under.
    pub name: String,
    /// The moment to fork from, when the fork comes from a backup set.
    ///
    /// `None` forks the live parent. A moment forks the newest backup
    /// set taken at or before it.
    pub at: Option<String>,
}

/// Where a branch's contents came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchSource {
    /// Forked from the live database.
    Live,
    /// Forked from a backup set at a moment in the past.
    Snapshot {
        /// The backup set's timestamp, as the backup root names it.
        set: String,
    },
}

/// A branch's recorded facts.
///
/// The registry is the durable half of a branch: the copy itself is an
/// ordinary database in the store, and this says where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// The name of the branched database.
    pub name: String,
    /// The database it was forked from.
    pub parent: String,
    /// When the fork was taken, in seconds since the Unix epoch.
    pub forked_at: u64,
    /// Where the fork's contents came from.
    pub source: BranchSource,
}

/// What a fork produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForkOutcome {
    /// The name the fork was published under.
    pub name: String,
    /// The database it was forked from.
    pub parent: String,
    /// When the fork was taken, in seconds since the Unix epoch.
    pub forked_at: u64,
    /// Where the fork's contents came from.
    pub source: BranchSource,
    /// The size of the forked database file, in bytes.
    pub size_bytes: u64,
}

/// The body of `POST /v1/branch`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkBody {
    parent: String,
    name: String,
    /// The moment to fork from; absent or null forks the live parent.
    #[serde(default)]
    at: Option<String>,
}

/// Fork a database and record the fork.
///
/// The destination must be a NEW name: a fork that landed on a live
/// database would be a silent destruction, and the registry is shared
/// by every database the gateway holds.
///
/// The fork takes the destination's write lane and runs under the
/// freshness fence, exactly like a clone: the new name is a database
/// like any other, and the backup status of that name must not be
/// ambiguous because the copy was not fenced.
pub fn fork(store: &Store, principal: &Principal, request: ForkRequest) -> Result<ForkOutcome> {
    // A fork publishes a NEW database in a shared registry, so it is
    // owner-only: the same rule clone, create and import follow.
    principal.owner()?;
    crate::store::validate_name(&request.parent)?;
    crate::store::validate_name(&request.name)?;
    if request.name == request.parent {
        return Err(Error::new(
            409,
            "database_exists",
            "a database cannot be forked onto itself",
        ));
    }
    // The destination has to be free: a fork that landed on a live
    // database would be a silent destruction.
    ensure_absent(store, &request.name)?;

    let name = request.name.clone();
    let forked = request.clone();
    let outcome = std::cell::RefCell::new(None);
    let _guard = store.acquire_queue(&name, true)?;
    store.fenced_write(&name, |store| {
        let forked = fork_inner(store, forked)?;
        *outcome.borrow_mut() = Some(forked);
        Ok(())
    })?;
    outcome.into_inner().ok_or_else(Error::internal)
}

/// The copy itself, without the lane and the fence.
fn fork_inner(store: &Store, request: ForkRequest) -> Result<ForkOutcome> {
    let source = match request.at.as_deref() {
        None => {
            // The live parent, copied with the SQLite backup primitive --
            // the same copy `dg clone` makes.
            store.path(&request.parent)?;
            store.clone_database(&request.parent, &request.name)?;
            BranchSource::Live
        }
        Some(at) => {
            // A snapshot carries the contents, so the parent need not
            // still be there: forking from last Tuesday's set is how a
            // database that was deleted yesterday comes back.
            let moment = parse_moment(at)?;
            let root = backup_root(store)?;
            let set = choose_set(store, &root, moment, at)?;
            let file = verified_member(&root, &set, &request.parent)?;
            store.import_from(&request.name, &file)?;
            BranchSource::Snapshot { set }
        }
    };
    let forked_at = now_secs();
    let set = match &source {
        BranchSource::Live => None,
        BranchSource::Snapshot { set } => Some(set.clone()),
    };
    let record = BranchRecord {
        name: request.name.clone(),
        parent: request.parent.clone(),
        forked_at,
        set,
    };
    store.branches.record(record);
    if let Err(error) = store.branches.persist(store.root()) {
        // The fork exists; the note about where it came from does not.
        // That is worth a loud failure, not a quiet one: an operator
        // who is told the fork failed will look, and an operator who is
        // not told will not.
        tracing::error!(
            code = error.code,
            detail = %error,
            "the branch record could not be written; the fork exists but its origin is not recorded"
        );
        return Err(Error::with_message(
            500,
            "branch_record_unwritten",
            format!(
                "the fork of {} was made, but its branch record could not be written: {error}",
                request.name
            ),
        ));
    }
    let size_bytes = std::fs::metadata(store.root().join(format!("{}.sqlite", request.name)))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    Ok(ForkOutcome {
        name: request.name,
        parent: request.parent,
        forked_at,
        source,
        size_bytes,
    })
}

/// Every branch the registry holds, by name.
pub fn branches(store: &Store) -> Vec<Branch> {
    store.branches.all().iter().map(Branch::from).collect()
}

/// One branch, by name.
pub fn branch(store: &Store, name: &str) -> Result<Branch> {
    let record = store
        .branches
        .get(name)
        .ok_or_else(|| Error::new(404, "branch_not_found", "branch not found"))?;
    Ok(Branch::from(&record))
}

impl From<&BranchRecord> for Branch {
    fn from(record: &BranchRecord) -> Self {
        Branch {
            name: record.name.clone(),
            parent: record.parent.clone(),
            forked_at: record.forked_at,
            source: match &record.set {
                Some(set) => BranchSource::Snapshot { set: set.clone() },
                None => BranchSource::Live,
            },
        }
    }
}

/// The backup root, when the gateway has one.
fn backup_root(store: &Store) -> Result<PathBuf> {
    store.backup_root().ok_or_else(|| {
        Error::new(
            409,
            "branch_source_unavailable",
            "this gateway has no backup root, so there is no past to fork from",
        )
    })
}

/// The newest backup set taken at or before `moment`.
///
/// The set's own NAME is the moment it was taken, so the window is
/// decided by the names and not by filesystem timestamps: a set that
/// was copied around by a backup tool is still the set it claims to
/// be.
///
/// When no set is old enough the error names the window that IS
/// available. Guessing the newest set would quietly fork the wrong
/// moment, and an operator who asked for Tuesday would have no way to
/// tell.
fn choose_set(
    store: &Store,
    root: &std::path::Path,
    moment: u64,
    requested: &str,
) -> Result<String> {
    let Some(sets) = store.backup_sets() else {
        return Err(Error::with_message(
            409,
            "branch_source_unavailable",
            format!(
                "the backup root at {} could not be read, so there is no past to fork from",
                root.display()
            ),
        ));
    };
    let mut stamps: Vec<String> = sets.iter().map(|set| set.timestamp.clone()).collect();
    stamps.sort();
    if stamps.is_empty() {
        return Err(Error::with_message(
            409,
            "branch_source_unavailable",
            format!(
                "the backup root at {} holds no sets, so there is no past to fork from",
                root.display()
            ),
        ));
    }
    let chosen = stamps
        .iter()
        .rfind(|stamp| set_epoch(stamp).is_some_and(|epoch| epoch <= moment))
        .cloned();
    let Some(chosen) = chosen else {
        let oldest = stamps.first().cloned().unwrap_or_default();
        let newest = stamps.last().cloned().unwrap_or_default();
        return Err(Error::with_message(
            409,
            "branch_source_unavailable",
            format!(
                "no backup set was taken at or before {requested}; the available window is {oldest} to {newest}"
            ),
        ));
    };
    Ok(chosen)
}

/// The file the set holds for `database`, after the set has verified.
///
/// The set is verified with the same helper `dg backup verify` uses:
/// the manifest, the `.sha256` sidecars and the files themselves have
/// to agree, and a set that does not verify is refused rather than
/// copied. A set that carries a broken member is refused whole -- the
/// copy is the operator's evidence, and evidence with a hole in it is
/// not evidence.
fn verified_member(root: &std::path::Path, set: &str, database: &str) -> Result<PathBuf> {
    let report = crate::backup::verify_set(root, set)?;
    if !report.ok {
        tracing::warn!(
            set,
            failures = report.failures.len(),
            "the backup set does not verify; refusing to fork from it"
        );
        return Err(Error::with_message(
            409,
            "branch_source_unverified",
            format!(
                "the backup set {set} does not verify, so it cannot be forked from: {}",
                report.failures.join("; ")
            ),
        ));
    }
    let member = report
        .databases
        .iter()
        .find(|member| member.name == database)
        .ok_or_else(|| {
            Error::with_message(
                409,
                "branch_source_missing_database",
                format!("the backup set {set} does not contain {database}"),
            )
        })?;
    Ok(root.join(set).join(&member.file))
}

/// The moment a backup set was taken, from the name the backup root
/// gives it (`20261016T000000Z`).
fn set_epoch(stamp: &str) -> Option<u64> {
    let bytes = stamp.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return None;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 8 || index == 15 || byte.is_ascii_digit())
    {
        return None;
    }
    // The bytes are ASCII digits now, so these slices are on character
    // boundaries.
    let year: i64 = stamp[0..4].parse().ok()?;
    let month: i64 = stamp[4..6].parse().ok()?;
    let day: i64 = stamp[6..8].parse().ok()?;
    let hour: i64 = stamp[9..11].parse().ok()?;
    let minute: i64 = stamp[11..13].parse().ok()?;
    let second: i64 = stamp[13..15].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let epoch = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(epoch).ok()
}

/// A moment in time, in seconds since the Unix epoch.
///
/// The forms an operator is likely to have are accepted: ISO 8601 with
/// or without a zone, a plain date, and the compact stamp the backup
/// sets are named with. A moment without a zone is read as UTC, which
/// is what every other timestamp in this gateway means.
pub fn parse_moment(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(invalid_moment(text, "the moment is empty"));
    }
    if let Some(epoch) = set_epoch(trimmed) {
        return Ok(epoch);
    }
    let bytes = trimmed.as_bytes();
    let (body, offset) = match trimmed
        .strip_suffix('Z')
        .or_else(|| trimmed.strip_suffix('z'))
    {
        Some(stripped) => (stripped, 0i64),
        None => {
            // An offset sign can only appear after the date, so the
            // search starts past it.
            let sign_at = (11..bytes.len())
                .rev()
                .find(|index| matches!(bytes[*index], b'+' | b'-'));
            match sign_at {
                Some(index) => {
                    let (head, tail) = trimmed.split_at(index);
                    let sign = if bytes[index] == b'-' { -1 } else { 1 };
                    (head, sign * parse_offset(&tail[1..])?)
                }
                None => (trimmed, 0),
            }
        }
    };
    let (date, time) = match body.find(['T', 't', ' ']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let (year, month, day) = parse_date(date, trimmed)?;
    let (hour, minute, second) = match time {
        Some(time) => parse_time(time, trimmed)?,
        None => (0, 0, 0),
    };
    let epoch =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    u64::try_from(epoch).map_err(|_| invalid_moment(trimmed, "the moment is before 1970"))
}

fn parse_offset(text: &str) -> Result<i64> {
    let digits: Vec<u8> = text.bytes().filter(|byte| *byte != b':').collect();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(invalid_moment(text, "the zone offset is not a number"));
    }
    let hours = digits
        .iter()
        .take(2)
        .fold(0i64, |value, digit| value * 10 + i64::from(digit - b'0'));
    let minutes = if digits.len() >= 4 {
        digits[2..4]
            .iter()
            .fold(0i64, |value, digit| value * 10 + i64::from(digit - b'0'))
    } else {
        0
    };
    if hours > 23 || minutes > 59 {
        return Err(invalid_moment(text, "the zone offset is out of range"));
    }
    Ok(hours * 3_600 + minutes * 60)
}

fn parse_date(text: &str, original: &str) -> Result<(i64, i64, i64)> {
    let bytes = text.as_bytes();
    let (year, month, day) = if bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        (&text[0..4], &text[5..7], &text[8..10])
    } else if bytes.len() == 8 && bytes.iter().all(u8::is_ascii_digit) {
        (&text[0..4], &text[4..6], &text[6..8])
    } else {
        return Err(invalid_moment(original, "the date is not YYYY-MM-DD"));
    };
    let year: i64 = year
        .parse()
        .map_err(|_| invalid_moment(original, "the year is not a number"))?;
    let month: i64 = month
        .parse()
        .map_err(|_| invalid_moment(original, "the month is not a number"))?;
    let day: i64 = day
        .parse()
        .map_err(|_| invalid_moment(original, "the day is not a number"))?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(invalid_moment(original, "the date is out of range"));
    }
    Ok((year, month, day))
}

fn parse_time(text: &str, original: &str) -> Result<(i64, i64, i64)> {
    let main = match text.split_once('.') {
        Some((main, fraction)) => {
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid_moment(original, "the fraction is not a number"));
            }
            main
        }
        None => text,
    };
    let parts: Vec<&str> = main.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(invalid_moment(original, "the time is not HH:MM[:SS]"));
    }
    let mut numbers = Vec::with_capacity(3);
    for part in &parts {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_moment(original, "the time is not a number"));
        }
        numbers.push(part.parse::<i64>().unwrap_or(i64::MAX));
    }
    let (hour, minute, second) = (numbers[0], numbers[1], numbers[2]);
    if hour > 23 || minute > 59 || second > 59 {
        return Err(invalid_moment(original, "the time is out of range"));
    }
    Ok((hour, minute, second))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_of_era = (month + 9) % 12;
    let day_of_year = (153 * month_of_era + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn invalid_moment(text: &str, reason: &str) -> Error {
    Error::with_message(
        400,
        "invalid_moment",
        format!("'{text}' is not a moment this gateway can read: {reason}"),
    )
}

/// The name is free: no live database and no tombstone holds it.
fn ensure_absent(store: &Store, name: &str) -> Result<()> {
    match store.path(name) {
        Ok(_) => Err(Error::new(409, "database_exists", "database exists")),
        // A name that is not registered is free. A name whose file has
        // gone missing is not: the registry holds it, and the operator
        // has to be able to see that.
        Err(error) if error.status == 404 => Ok(()),
        Err(error) => Err(error),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// The branch endpoints.
///
/// Merge this into the API router at startup:
///
/// ```ignore
/// app = app.merge(branch::router(store.clone()));
/// ```
///
/// The durable registry is read when the router is built, so a gateway
/// that restarts still knows which databases are forks.
pub fn router(store: Arc<Store>) -> Router {
    if let Err(error) = store.branches.load_into(store.root()) {
        tracing::error!(
            code = error.code,
            detail = %error,
            "the branch registry could not be read; this gateway starts with no branches"
        );
    }
    Router::new()
        .route("/v1/branch", post(create))
        .route("/v1/branches", get(list))
        .route("/v1/branches/{name}", get(show).delete(remove))
        .with_state(store)
}

/// Fork a database.
async fn create(
    State(store): State<Arc<Store>>,
    headers: HeaderMap,
    body: std::result::Result<Json<ForkBody>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = json_body(body)?;
    work(store, headers, move |s, p| {
        let outcome = fork(
            s,
            &p,
            ForkRequest {
                parent: body.parent,
                name: body.name,
                at: body.at,
            },
        )?;
        Ok(Json(
            serde_json::to_value(outcome).map_err(|_| Error::internal())?,
        ))
    })
    .await
}

/// Every branch the gateway knows about.
async fn list(State(store): State<Arc<Store>>, headers: HeaderMap) -> Result<Json<Value>> {
    work_read(store, headers, move |s, p| {
        // The branch list is operator information about the whole
        // registry, so an app key is refused: a scoped credential
        // learns nothing about databases it was not given.
        p.owner()?;
        let branches: Vec<Branch> = branches(s);
        Ok(Json(serde_json::json!({ "branches": branches })))
    })
    .await
}

/// One branch.
async fn show(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work_read(store, headers, move |s, p| {
        p.owner()?;
        let branch = branch(s, &name)?;
        Ok(Json(
            serde_json::to_value(branch).map_err(|_| Error::internal())?,
        ))
    })
    .await
}

/// Delete a branched database and forget the branch.
///
/// Both halves happen here, in one request: a record that outlived its
/// database would describe a fork that no longer exists, and a database
/// that outlived its record would be a fork nobody can name. The
/// database is tombstoned on the same terms as any other deletion.
async fn remove(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    work(store, headers, move |s, p| {
        p.owner()?;
        let record = s
            .branches
            .get(&name)
            .ok_or_else(|| Error::new(404, "branch_not_found", "branch not found"))?;
        let _guard = s.acquire_queue(&name, true)?;
        // The database is tombstoned first, and a database that was
        // ALREADY tombstoned -- by `dg delete`, say -- answers that with
        // 409. The branch is gone either way: its database is not
        // there to reach. So the record is forgotten even then, because
        // a registry entry that names a database nobody can reach is a
        // branch the operator can neither use nor list away.
        s.fenced_write(&name, |s| s.delete(&name, crate::api::RETENTION_SECS))?;
        s.branches.forget(&name);
        if let Err(error) = s.branches.persist(s.root()) {
            tracing::error!(
                code = error.code,
                detail = %error,
                "the branch record could not be removed from the registry"
            );
            return Err(Error::with_message(
                500,
                "branch_record_unwritten",
                format!(
                    "the branch {name} was deleted, but its record could not be removed: {error}"
                ),
            ));
        }
        Ok(Json(serde_json::json!({
            "deleted": true,
            "name": name,
            "parent": record.parent,
            "purge_after_seconds": crate::api::RETENTION_SECS,
        })))
    })
    .await
}

/// The credential from the request.
///
/// The branch endpoints carry their own extraction rather than sharing
/// the API's: this router is merged into it, and the API's helper is
/// private to that module.
fn credential(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(unauthorized)
}

fn json_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value).map_err(|error| {
        // Schema errors are bad requests in our public API, not Axum's
        // 422. Other categories (413 size, 415 content type) keep
        // their own status.
        let status = match &error {
            JsonRejection::JsonDataError(_) => 400,
            _ => error.status().as_u16(),
        };
        Error::new(
            status,
            "invalid_request",
            "malformed or oversized JSON body",
        )
    })
}

/// Run a write under the write lane.
async fn work<T: Send + 'static>(
    store: Arc<Store>,
    headers: HeaderMap,
    f: impl FnOnce(&Store, Principal) -> Result<T> + Send + 'static,
) -> Result<T> {
    work_in(store, headers, false, f).await
}

/// Run a read under the read lane.
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
    // A bounded wait instead of an instant shed: bursty arrivals queue
    // up to ACQUIRE_TIMEOUT, and overload still surfaces as the same
    // 429. The metrics clone exists so every exit below -- including
    // the lane sheds -- is counted.
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
    if let Err(error) = &result {
        metrics.metrics.record_error(error.code);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same instant, in the forms an operator is likely to have.
    #[test]
    fn a_moment_is_read_in_the_forms_an_operator_has() {
        let expected = parse_moment("2026-10-16T00:00:00Z").unwrap();
        assert_eq!(parse_moment("2026-10-16T00:00:00").unwrap(), expected);
        assert_eq!(parse_moment("2026-10-16 00:00:00").unwrap(), expected);
        assert_eq!(parse_moment("2026-10-16").unwrap(), expected);
        assert_eq!(parse_moment("20261016T000000Z").unwrap(), expected);
        assert_eq!(parse_moment("2026-10-16T01:00:00+01:00").unwrap(), expected);
        assert_eq!(parse_moment("2026-10-15T23:00:00-01:00").unwrap(), expected);
    }

    /// A moment that is not a moment is refused, not guessed at.
    #[test]
    fn a_moment_that_is_not_one_is_refused() {
        for text in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-10-32T00:00:00Z",
            "2026-10-16T25:00:00Z",
            "2026-10-16T00:00:00+99:00",
            "2026-10-16T00:00:00Z trailing",
        ] {
            assert!(parse_moment(text).is_err(), "'{text}' was read as a moment");
        }
    }

    /// A set's name is the moment it was taken.
    #[test]
    fn a_set_stamp_is_the_moment_it_names() {
        let moment = parse_moment("2026-10-16T00:00:00Z").unwrap();
        assert_eq!(set_epoch("20261016T000000Z"), Some(moment));
        assert_eq!(set_epoch("20261016T000000"), None);
        assert_eq!(set_epoch("not-a-stamp"), None);
    }
}
