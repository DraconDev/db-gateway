//! What differs between two databases.
//!
//! WHY the comparison runs here and not on the client: the answer to
//! "are these two databases the same?" is a fact about two files on this
//! host, and nothing about it needs the data to travel. Both files are
//! opened read-only, side by side, and compared in place, so a diff can
//! never become a way to read a database the credential could not
//! otherwise read.
//!
//! WHY the report says what it could NOT do: a comparison that stops
//! early, or that cannot line two tables up against each other, has not
//! found the databases equal. Every table that was not fully compared is
//! reported as not compared, with the reason, and the verdict stays
//! false. A diff that says "identical" means the comparison finished and
//! everything agreed.
//!
//! WHAT THE COMPARISON MEANS, exactly:
//!
//! * Schema: the tables, indexes, views and triggers of the two
//!   `sqlite_master`s, and for a table its columns (name, declared type,
//!   NOT NULL, default, primary key) and its foreign keys. Two spellings
//!   of the same definition -- one quoted, one not -- are the same
//!   schema; the definitions are compared as tokens for that reason.
//! * Rows: the rows of a table that exists on both sides, compared as
//!   MULTISETS of their canonical text form. The canonical form is the
//!   stored value of each column with its storage class, so an INTEGER 1
//!   and a REAL 1.0 are different rows here even though SQLite compares
//!   them equal numerically, and two TEXT values that differ only in
//!   their bytes are different rows. A BLOB is compared by its bytes.
//!   Comparing multisets rather than ordered sequences is what makes the
//!   answer independent of the order the rows happen to sit in.
//! * The verdict: `identical` is true only when the schemas matched and
//!   every table's rows matched and nothing was truncated.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
    routing::post,
};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::{ACQUIRE_TIMEOUT, Principal, Store, unauthorized};

/// The most rows one table contributes to a comparison.
///
/// A diff has to answer for a database with a million rows in the time an
/// operator is willing to wait for an answer. Past this cap the content
/// comparison stops and the report says so: a table that was not
/// finished is never reported as equal.
pub const MAX_ROWS_PER_TABLE: usize = 10_000;

/// The two databases a comparison is about.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffRequest {
    /// The database on the left of the comparison.
    pub left: String,
    /// The database on the right of the comparison.
    pub right: String,
}

/// What the comparison found.
///
/// `identical` is the verdict. It is true only when the schemas matched,
/// every table's rows matched, and nothing was left uncompared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffReport {
    /// The database on the left of the comparison.
    pub left: String,
    /// The database on the right of the comparison.
    pub right: String,
    /// How the two schemas differ.
    pub schema: SchemaDiff,
    /// How the rows of every table the two databases share compare.
    pub tables: Vec<TableDiff>,
    /// Whether the two databases are the same.
    pub identical: bool,
    /// How many rows were compared, across every table.
    pub compared_rows: u64,
    /// Whether any table hit the row cap.
    pub truncated: bool,
}

/// How the two schemas differ.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemaDiff {
    /// Whether the two schemas are the same.
    pub identical: bool,
    /// The objects only the left database has.
    pub only_left: Vec<SchemaObject>,
    /// The objects only the right database has.
    pub only_right: Vec<SchemaObject>,
    /// The objects both databases have, but not the same.
    pub changed: Vec<SchemaChange>,
}

/// One schema object: a table, an index, a view or a trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaObject {
    /// The name, as SQLite stores it.
    pub name: String,
    /// What kind of object it is.
    pub kind: String,
}

/// One schema object that exists on both sides and does not match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaChange {
    /// The name, as the left database spells it.
    pub name: String,
    /// What kind of object it is.
    pub kind: String,
    /// What differs, in words an operator can act on.
    pub detail: String,
}

/// How one table's rows compare.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDiff {
    /// The table's name.
    pub name: String,
    /// What the comparison established.
    pub status: TableStatus,
    /// How many rows the left database holds, when it could be counted.
    pub left_rows: Option<u64>,
    /// How many rows the right database holds, when it could be counted.
    pub right_rows: Option<u64>,
    /// How many rows were actually compared.
    pub compared_rows: u64,
    /// Whether the comparison stopped at the row cap.
    pub truncated: bool,
    /// How many rows differ, when the rows were compared: the number of
    /// rows that would have to change to make the two tables the same.
    pub differing_rows: Option<u64>,
    /// Why the rows were not compared, when they were not.
    pub reason: Option<String>,
}

/// What the comparison established about a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableStatus {
    /// The rows were compared and they are the same.
    Equal,
    /// The rows are different. A difference is always a fact: a row
    /// count that differs, or a full comparison that found rows that do
    /// not match.
    Different,
    /// The rows were NOT compared, and the reason says why. This is not
    /// a kind of "different": it is the absence of an answer.
    NotCompared,
}

/// Compare two databases and report what differs.
///
/// The caller must be allowed to read BOTH databases: the comparison is
/// refused otherwise, because a diff that could read one side of a pair
/// the caller may not see would hand out what the other side holds.
///
/// The two files are opened read-only, so a comparison cannot change
/// what it compares. Each file is read inside its own read transaction,
/// so every statement sees the same snapshot of THAT file -- but two
/// files are not one atomic snapshot of the pair: a write that lands
/// between the two opens is not in the report.
pub fn compare(store: &Store, principal: &Principal, request: DiffRequest) -> Result<DiffReport> {
    // Both sides are authorized before either is opened: a refusal must
    // not depend on which side the caller named first.
    store.authorize(principal, &request.left)?;
    store.authorize(principal, &request.right)?;
    let left_path = store.path(&request.left)?;
    let right_path = store.path(&request.right)?;

    let left = open_read_only(&left_path)?;
    let right = open_read_only(&right_path)?;
    // One read transaction per file, so the whole comparison sees one
    // snapshot of each file rather than a moving target.
    left.execute_batch("BEGIN").map_err(unreadable)?;
    right.execute_batch("BEGIN").map_err(unreadable)?;

    let left_schema = read_schema(&left)?;
    let right_schema = read_schema(&right)?;
    let schema = diff_schemas(&left_schema, &right_schema);

    let mut tables = Vec::new();
    let mut compared_rows = 0u64;
    let mut truncated = false;
    for (key, object) in &left_schema {
        let Some(other) = right_schema.get(key) else {
            // Only on one side: the schema difference already says so.
            continue;
        };
        if object.kind != "table" {
            continue;
        }
        let diff = compare_table(&left, &right, object, other)?;
        compared_rows = compared_rows.saturating_add(diff.compared_rows);
        truncated = truncated || diff.truncated;
        tables.push(diff);
    }

    let identical = schema.identical
        && !truncated
        && tables
            .iter()
            .all(|table| table.status == TableStatus::Equal);
    Ok(DiffReport {
        left: request.left,
        right: request.right,
        schema,
        tables,
        identical,
        compared_rows,
        truncated,
    })
}

/// The route that answers a comparison.
///
/// `POST /v1/diff` with `{"left": "...", "right": "..."}` and the report
/// as JSON. The caller must be allowed to read both databases.
pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/diff", post(diff))
        .with_state(store)
}

/// The comparison endpoint.
async fn diff(
    State(store): State<Arc<Store>>,
    headers: HeaderMap,
    body: std::result::Result<Json<DiffRequest>, JsonRejection>,
) -> Result<Json<DiffReport>> {
    let request = json_body(body)?;
    // The credential is read here rather than borrowed from the rest of
    // the API: the diff is a separate router, and a bearer credential is
    // a bearer credential.
    let key = credential(&headers)?;
    // A diff is a READ. It takes a read-lane permit and never the write
    // lane, so a comparison cannot shut writers out of the service.
    let permit = match tokio::time::timeout(
        ACQUIRE_TIMEOUT,
        store.read_lane.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        _ => return Err(Error::busy()),
    };
    let metrics = store.clone();
    metrics.metrics.record_request();
    let store_for_task = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // The lifecycle lease keeps a purge from unlinking a file while
        // it is being compared.
        let _lease = store_for_task.request_lease()?;
        let principal = store_for_task.authenticate(&key)?;
        compare(&store_for_task, &principal, request)
    })
    .await
    .map_err(|_| Error::internal())?;
    if let Err(error) = &result {
        store.metrics.record_error(error.code);
    }
    Ok(Json(result?))
}

/// The bearer credential, or a refusal.
fn credential(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(unauthorized)
}

/// The request body, or a refusal.
fn json_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value).map_err(|error| {
        // A schema error is a bad request in this API, not Axum's 422.
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

/// Open a database for reading and nothing else.
///
/// Read-only is the point: a comparison that could write to what it
/// compares could change the answer it is about to give.
fn open_read_only(path: &std::path::Path) -> Result<Connection> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(unreadable)?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(unreadable)?;
    Ok(conn)
}

/// A file that could not be read is the gateway's problem, not the
/// caller's request.
fn unreadable(error: rusqlite::Error) -> Error {
    tracing::error!(
        code = "diff_read_failed",
        reason = %error,
        "cannot read a database for comparison"
    );
    Error::new(
        500,
        "database_unreadable",
        "a database could not be read for comparison",
    )
}

/// One schema object as SQLite describes it.
struct Object {
    /// The name, as SQLite stores it.
    name: String,
    /// "table", "index", "view" or "trigger".
    kind: String,
    /// The columns of a table, in order.
    columns: Vec<Column>,
    /// The foreign keys of a table.
    foreign_keys: Vec<String>,
    /// The definition, as tokens.
    definition: Vec<String>,
}

/// One column of a table, as SQLite describes it.
struct Column {
    /// The column's name.
    name: String,
    /// The declared type, upper-cased; empty when the column has none.
    kind: String,
    /// Whether the column was declared NOT NULL.
    notnull: bool,
    /// The default value expression, whitespace collapsed.
    default: String,
    /// The column's primary-key position.
    pk: i64,
}

impl Column {
    /// The column as it is compared: everything SQLite reports about it.
    fn signature(&self) -> String {
        format!(
            "{} {} notnull={} default={} pk={}",
            self.name, self.kind, self.notnull, self.default, self.pk
        )
    }

    /// The column as an operator reads it: "id INTEGER".
    fn summary(&self) -> String {
        if self.kind.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.kind)
        }
    }
}

/// The schema of one database, keyed by lower-cased name.
///
/// SQLite names are case-insensitive, so `Users` and `users` are the
/// same object; the key is the lower-cased name and the original
/// spelling is kept for the report.
fn read_schema(conn: &Connection) -> Result<BTreeMap<String, Object>> {
    let mut stmt = conn
        .prepare(
            "SELECT name, type, COALESCE(sql,'') FROM sqlite_master \
             WHERE type IN ('table','index','view','trigger') \
               AND lower(substr(name,1,7)) <> 'sqlite_' \
               AND lower(substr(name,1,4)) <> '_dg_' \
             ORDER BY name",
        )
        .map_err(unreadable)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(unreadable)?;
    let mut objects = Vec::new();
    for row in rows {
        objects.push(row.map_err(unreadable)?);
    }
    drop(stmt);

    let mut schema = BTreeMap::new();
    for (name, kind, sql) in objects {
        let (columns, foreign_keys) = if kind == "table" {
            (read_columns(conn, &name)?, read_foreign_keys(conn, &name)?)
        } else {
            (Vec::new(), Vec::new())
        };
        schema.insert(
            name.to_lowercase(),
            Object {
                name,
                kind,
                columns,
                foreign_keys,
                definition: sql_tokens(&sql),
            },
        );
    }
    Ok(schema)
}

/// The columns of a table, in order.
fn read_columns(conn: &Connection, table: &str) -> Result<Vec<Column>> {
    let mut stmt = conn
        .prepare(
            "SELECT name, type, \"notnull\", COALESCE(dflt_value,''), pk \
             FROM pragma_table_info(?1)",
        )
        .map_err(unreadable)?;
    let rows = stmt
        .query_map([table], |row| {
            Ok(Column {
                name: row.get(0)?,
                kind: row.get::<_, String>(1)?.trim().to_uppercase(),
                notnull: row.get::<_, i64>(2)? != 0,
                default: collapse_whitespace(&row.get::<_, String>(3)?),
                pk: row.get(4)?,
            })
        })
        .map_err(unreadable)?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row.map_err(unreadable)?);
    }
    Ok(columns)
}

/// The foreign keys of a table, in the order SQLite reports them.
fn read_foreign_keys(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT \"table\", \"from\", \"to\", on_update, on_delete, \"match\" \
             FROM pragma_foreign_key_list(?1) ORDER BY id",
        )
        .map_err(unreadable)?;
    let rows = stmt
        .query_map([table], |row| {
            Ok(format!(
                "{}.{} -> {}.{} on_update={} on_delete={} match={}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(0)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(unreadable)?;
    let mut keys = Vec::new();
    for row in rows {
        keys.push(row.map_err(unreadable)?);
    }
    Ok(keys)
}

/// The differences between two schemas.
fn diff_schemas(left: &BTreeMap<String, Object>, right: &BTreeMap<String, Object>) -> SchemaDiff {
    let mut only_left = Vec::new();
    let mut only_right = Vec::new();
    let mut changed = Vec::new();
    for (key, object) in left {
        match right.get(key) {
            None => only_left.push(SchemaObject {
                name: object.name.clone(),
                kind: object.kind.clone(),
            }),
            Some(other) => {
                if let Some(detail) = object_difference(object, other) {
                    changed.push(SchemaChange {
                        name: object.name.clone(),
                        kind: object.kind.clone(),
                        detail,
                    });
                }
            }
        }
    }
    for (key, object) in right {
        if !left.contains_key(key) {
            only_right.push(SchemaObject {
                name: object.name.clone(),
                kind: object.kind.clone(),
            });
        }
    }
    SchemaDiff {
        identical: only_left.is_empty() && only_right.is_empty() && changed.is_empty(),
        only_left,
        only_right,
        changed,
    }
}

/// What differs between two objects that exist on both sides.
fn object_difference(left: &Object, right: &Object) -> Option<String> {
    let left_columns: Vec<String> = left.columns.iter().map(Column::signature).collect();
    let right_columns: Vec<String> = right.columns.iter().map(Column::signature).collect();
    if left_columns != right_columns {
        let names = |columns: &[Column]| {
            columns
                .iter()
                .map(Column::summary)
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Some(format!(
            "columns differ: left [{}], right [{}]",
            names(&left.columns),
            names(&right.columns)
        ));
    }
    if left.foreign_keys != right.foreign_keys {
        return Some("foreign keys differ".to_string());
    }
    if left.definition != right.definition {
        return Some("definition differs".to_string());
    }
    None
}

/// Compare the rows of one table that exists on both sides.
fn compare_table(
    left: &Connection,
    right: &Connection,
    left_object: &Object,
    right_object: &Object,
) -> Result<TableDiff> {
    let name = left_object.name.clone();
    let left_columns: Vec<String> = left_object.columns.iter().map(Column::signature).collect();
    let right_columns: Vec<String> = right_object.columns.iter().map(Column::signature).collect();

    let left_rows = count_rows(left, &name);
    let right_rows = count_rows(right, &name);
    let (left_rows, right_rows) = match (left_rows, right_rows) {
        (Ok(left_rows), Ok(right_rows)) => (Some(left_rows), Some(right_rows)),
        (left, right) => {
            // The rows could not even be counted: that is not a
            // difference, it is the absence of an answer.
            let reason = left
                .err()
                .or(right.err())
                .map(|error| error.to_string())
                .unwrap_or_default();
            return Ok(TableDiff {
                name,
                status: TableStatus::NotCompared,
                left_rows: None,
                right_rows: None,
                compared_rows: 0,
                truncated: false,
                differing_rows: None,
                reason: Some(format!("the rows could not be counted: {reason}")),
            });
        }
    };

    if left_columns != right_columns {
        // Two tables with different columns cannot be lined up against
        // each other: a row of one is not a row of the other.
        return Ok(TableDiff {
            name,
            status: TableStatus::NotCompared,
            left_rows,
            right_rows,
            compared_rows: 0,
            truncated: false,
            differing_rows: None,
            reason: Some("the column lists differ, so the rows cannot be lined up".to_string()),
        });
    }

    let left_count = left_rows.unwrap_or_default();
    let right_count = right_rows.unwrap_or_default();
    if left_count != right_count {
        // The counts differ, so the tables differ. Nothing more has to
        // be read to know that.
        return Ok(TableDiff {
            name,
            status: TableStatus::Different,
            left_rows,
            right_rows,
            compared_rows: 0,
            truncated: false,
            differing_rows: None,
            reason: Some(format!(
                "row counts differ: left {left_count}, right {right_count}"
            )),
        });
    }

    let count = left_count;
    if count as usize > MAX_ROWS_PER_TABLE {
        // Past the cap the content comparison stops. The counts are
        // equal, but equal counts are not equal rows, and a sample of
        // each side is not a proof either way.
        let left_sample = read_rows(left, &name, MAX_ROWS_PER_TABLE)?;
        let right_sample = read_rows(right, &name, MAX_ROWS_PER_TABLE)?;
        let sampled_differ = left_sample != right_sample;
        let mut reason =
            format!("content not compared: {count} rows is over the {MAX_ROWS_PER_TABLE}-row cap");
        reason.push_str(if sampled_differ {
            "; the sampled rows differ, which is a hint and not a proof"
        } else {
            "; the sampled rows match, which is not a proof"
        });
        return Ok(TableDiff {
            name,
            status: TableStatus::NotCompared,
            left_rows,
            right_rows,
            compared_rows: MAX_ROWS_PER_TABLE as u64,
            truncated: true,
            differing_rows: None,
            reason: Some(reason),
        });
    }

    let left_rows_read = read_rows(left, &name, MAX_ROWS_PER_TABLE)?;
    let right_rows_read = read_rows(right, &name, MAX_ROWS_PER_TABLE)?;
    let differing = multiset_difference(&left_rows_read, &right_rows_read);
    let status = if differing == 0 {
        TableStatus::Equal
    } else {
        TableStatus::Different
    };
    Ok(TableDiff {
        name,
        status,
        left_rows,
        right_rows,
        compared_rows: left_rows_read.len() as u64,
        truncated: false,
        differing_rows: Some(differing),
        reason: if differing == 0 {
            None
        } else {
            Some(format!("{differing} rows differ"))
        },
    })
}

/// How many rows a table holds.
fn count_rows(conn: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT count(*) FROM {}", quote_identifier(table));
    let count: i64 = conn
        .query_row(&sql, [], |row| row.get(0))
        .map_err(unreadable)?;
    Ok(count.max(0) as u64)
}

/// Read up to `limit` rows, each reduced to its canonical bytes.
fn read_rows(conn: &Connection, table: &str, limit: usize) -> Result<Vec<Vec<u8>>> {
    let sql = format!("SELECT * FROM {} LIMIT {}", quote_identifier(table), limit);
    let mut stmt = conn.prepare(&sql).map_err(unreadable)?;
    let mut rows = stmt.query([]).map_err(unreadable)?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next().map_err(unreadable)? {
        keys.push(row_key(row).map_err(unreadable)?);
    }
    keys.sort();
    Ok(keys)
}

/// The canonical bytes of one row.
///
/// Every value carries its storage class and, for text and blobs, its
/// length, so two different rows cannot produce the same bytes by
/// accident: the text "a;b" is not the text "a" followed by "b".
fn row_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<u8>> {
    let mut key = Vec::new();
    for index in 0..row.as_ref().column_count() {
        match row.get_ref(index)? {
            ValueRef::Null => key.extend_from_slice(b"N;"),
            ValueRef::Integer(value) => {
                key.extend_from_slice(b"I");
                key.extend_from_slice(value.to_string().as_bytes());
                key.push(b';');
            }
            ValueRef::Real(value) => {
                key.extend_from_slice(b"F");
                key.extend_from_slice(format!("{value:?}").as_bytes());
                key.push(b';');
            }
            ValueRef::Text(bytes) => {
                key.extend_from_slice(b"T");
                key.extend_from_slice(bytes.len().to_string().as_bytes());
                key.push(b':');
                key.extend_from_slice(bytes);
                key.push(b';');
            }
            ValueRef::Blob(bytes) => {
                key.extend_from_slice(b"B");
                key.extend_from_slice(bytes.len().to_string().as_bytes());
                key.push(b':');
                key.extend_from_slice(&hex_bytes(bytes));
                key.push(b';');
            }
        }
    }
    Ok(key)
}

/// The bytes of a blob, as hex.
fn hex_bytes(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize]);
        out.push(DIGITS[(byte & 0x0f) as usize]);
    }
    out
}

/// How many rows differ between two multisets.
///
/// Equal rows cancel out, so a table with three of a row and one with two
/// of it has no row in common. The count is the number of rows that would
/// have to change to make the two tables the same: a row that was replaced
/// counts once, and a row that only one side has counts once.
fn multiset_difference(left: &[Vec<u8>], right: &[Vec<u8>]) -> u64 {
    let mut only_left = 0u64;
    let mut only_right = 0u64;
    let mut left_index = 0usize;
    let mut right_index = 0usize;
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => {
                only_left += 1;
                left_index += 1;
            }
            std::cmp::Ordering::Greater => {
                only_right += 1;
                right_index += 1;
            }
            std::cmp::Ordering::Equal => {
                left_index += 1;
                right_index += 1;
            }
        }
    }
    only_left += (left.len() - left_index) as u64;
    only_right += (right.len() - right_index) as u64;
    only_left.max(only_right)
}

/// An identifier, quoted for SQLite.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The SQL as tokens, so a reformatting is not a difference.
///
/// Two spellings of the same definition -- one quoted, one not, one with
/// a space before the parenthesis -- hold the same schema, and reporting
/// that as a difference would make the report noise. Identifiers and
/// keywords are lower-cased; string literals are left exactly as they
/// were written, because 'A' and 'a' are different values.
fn sql_tokens(sql: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut characters = sql.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' | '`' => {
                flush_word(&mut word, &mut tokens);
                let mut quoted = String::new();
                for inner in characters.by_ref() {
                    if inner == character {
                        break;
                    }
                    quoted.push(inner);
                }
                tokens.push(quoted.to_lowercase());
            }
            '[' => {
                flush_word(&mut word, &mut tokens);
                let mut quoted = String::new();
                for inner in characters.by_ref() {
                    if inner == ']' {
                        break;
                    }
                    quoted.push(inner);
                }
                tokens.push(quoted.to_lowercase());
            }
            '\'' => {
                flush_word(&mut word, &mut tokens);
                let mut literal = String::new();
                for inner in characters.by_ref() {
                    literal.push(inner);
                    if inner == '\'' {
                        break;
                    }
                }
                tokens.push(literal);
            }
            character
                if character.is_alphanumeric()
                    || character == '_'
                    || character == '$'
                    || character == '.' =>
            {
                word.push(character);
            }
            character if character.is_whitespace() => flush_word(&mut word, &mut tokens),
            character => {
                flush_word(&mut word, &mut tokens);
                tokens.push(character.to_string());
            }
        }
    }
    flush_word(&mut word, &mut tokens);
    tokens
}

/// The pending word, as a token.
fn flush_word(word: &mut String, tokens: &mut Vec<String>) {
    if !word.is_empty() {
        tokens.push(std::mem::take(word).to_lowercase());
    }
}

/// Whitespace collapsed to single spaces.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
