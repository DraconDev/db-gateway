//! Typed Rust client for the DB Gateway JSON API.
//!
//! This is the Rust-side application access path (audit A6): production
//! writers in the platform are Rust and previously had only the
//! TypeScript client (`client/`) or the gateway CLI's untyped transport
//! to copy. The crate mirrors the TypeScript client's transport
//! contract:
//!
//! - origins must be HTTPS or a literal loopback HTTP origin; userinfo,
//!   query, fragment, and non-root paths are refused before any request;
//! - no proxies and no redirect following, so bearer credentials never
//!   leave the configured origin via ambient environment or a 30x;
//! - responses are size-bounded while streaming; a failed *write*
//!   transport surfaces as `outcome_unknown` (replay only with the same
//!   idempotency key), a failed read as `transport_unavailable`;
//! - a request is classified by the statements it sends, with the same
//!   rule the server uses, so a read-only query gives up in ten seconds
//!   and is never reported as a write that may have committed. (The
//!   TypeScript client classifies the same way.)
//! - no hidden retries: replay is an explicit caller decision via
//!   [`Database::execute_with_key`] / [`Database::batch_with_key`].
//!
//! Key lifecycle (issue/revoke/rotate) intentionally stays with the
//! owner CLI (`dg key ...`); applications hold one scoped key and use
//! the query surface below.

use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::time::Duration;

/// Default bounded response size: the gateway's 256 KiB result cap plus
/// JSON overhead, matching the CLI transport.
const DEFAULT_MAX_RESPONSE: usize = 256 * 1024 + 4096;

/// How long a read waits before the client gives up. A read that is given
/// up on costs a retry and nothing else.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a write waits before the client gives up. A write that is
/// given up on leaves the outcome in doubt, so it waits far longer.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether a failed request can have committed data.
///
/// The HTTP method cannot tell a read from a write — a query is a POST —
/// so the statements decide, with the SAME rule the server uses, so the
/// two agree. A read that is given up on costs a retry; a write that is
/// given up on leaves the outcome in doubt, and the client must not call
/// a read a write: telling an operator that a write may have committed
/// when nothing could have committed is its own kind of wrong answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Semantics {
    /// A read: no durable write can exist.
    Read,
    /// A write: a timeout leaves the outcome genuinely in doubt.
    Write,
}

/// The transport semantics of one statement.
pub fn semantics_of(sql: &str) -> Semantics {
    if is_read_only(sql) {
        Semantics::Read
    } else {
        Semantics::Write
    }
}

/// The transport semantics of a set of statements: a batch is a read only
/// when every statement in it is one.
pub fn semantics_of_statements(statements: &[Statement]) -> Semantics {
    if statements.iter().all(|s| is_read_only(&s.sql)) {
        Semantics::Read
    } else {
        Semantics::Write
    }
}

/// Whether a statement can only read.
///
/// The same rule the server uses for its admission lane: the statement
/// opens with SELECT/VALUES/EXPLAIN, or with WITH and holds no
/// data-modifying keyword. It fails closed — anything ambiguous takes the
/// write lane, which is slower for that request and never wrong.
pub fn is_read_only(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_ascii_uppercase();
    (upper.starts_with("SELECT")
        || upper.starts_with("VALUES")
        || upper.starts_with("EXPLAIN")
        || upper.starts_with("WITH"))
        && !has_write_keyword(trimmed)
}

/// Scan the SQL for a data-modifying keyword, ignoring anything that is not a
/// bare keyword token: single-quoted literals, double-quoted/backtick/bracket
/// quoted identifiers, and both comment forms.
///
/// The distinction is not cosmetic. `SELECT 'DELETE'` and `SELECT 1 -- DELETE`
/// write nothing, so classifying them as writes reports a read-only query as
/// `outcome_unknown` -- the exact wrong answer this classification exists to
/// prevent -- and makes the caller wait a minute instead of ten seconds. The
/// same tokenizer runs in `src/execution.rs` and `client/src/index.ts`; the
/// three must agree, and `tests/rust_client.rs` pins that.
///
/// It still fails closed in the direction that matters: an unterminated literal
/// or comment consumes the rest of the string, so a keyword hidden after one is
/// not seen (a read that SQLite would reject as a syntax error, never a write
/// that slips through).
fn has_write_keyword(sql: &str) -> bool {
    const WORDS: [&str; 9] = [
        "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "VACUUM", "REINDEX",
    ];
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        match bytes[i] {
            // -- line comment
            b'-' if i + 1 < n && bytes[i + 1] == b'-' => {
                i += 2;
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // /* block comment */ (SQLite does not nest these)
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = if i + 1 < n { i + 2 } else { n };
            }
            // 'string literal' -- '' is an escaped quote inside it
            b'\'' => {
                i += 1;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // "quoted" / `quoted` identifier: a keyword here is just a name
            quote @ (b'"' | b'`') => {
                i += 1;
                while i < n {
                    if bytes[i] == quote {
                        if i + 1 < n && bytes[i + 1] == quote {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // [bracket] identifier
            b'[' => {
                i += 1;
                while i < n && bytes[i] != b']' {
                    i += 1;
                }
                if i < n {
                    i += 1;
                }
            }
            c if c.is_ascii_alphabetic() => {
                let start = i;
                while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if WORDS.contains(&sql[start..i].to_ascii_uppercase().as_str()) {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}
/// Gateway database/credential name rule: `^[a-z0-9][a-z0-9_]{0,63}$`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || b.is_ascii_digit() || (i > 0 && b == b'_'))
        && name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// One SQL argument/result value, in the gateway's JSON encoding:
/// integers stay integers, blobs travel as byte arrays.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    fn to_json(&self) -> Json {
        match self {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Integer(i) => json!(*i),
            Value::Real(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
            Value::Text(s) => Json::String(s.clone()),
            Value::Blob(bytes) => json!(bytes),
        }
    }

    fn from_json(v: &Json) -> Option<Value> {
        match v {
            Json::Null => Some(Value::Null),
            Json::Bool(b) => Some(Value::Bool(*b)),
            Json::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(Value::Integer(i))
                } else {
                    n.as_f64().map(Value::Real)
                }
            }
            Json::String(s) => Some(Value::Text(s.clone())),
            Json::Array(items) => {
                let mut bytes = Vec::with_capacity(items.len());
                for item in items {
                    bytes.push(u8::try_from(item.as_u64()?).ok()?);
                }
                Some(Value::Blob(bytes))
            }
            Json::Object(_) => None,
        }
    }
}

/// One statement in a batch.
#[derive(Clone, Debug)]
pub struct Statement {
    pub sql: String,
    pub args: Vec<Value>,
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            args: Vec::new(),
        }
    }
    pub fn args(mut self, args: Vec<Value>) -> Self {
        self.args = args;
        self
    }
}

/// A validated query/batch statement result.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub rows_affected: u64,
}

fn parse_result(v: &Json) -> Option<QueryResult> {
    let columns: Vec<String> = v
        .get("columns")?
        .as_array()?
        .iter()
        .map(|c| c.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    let mut rows = Vec::new();
    for row in v.get("rows")?.as_array()? {
        let cells = row.as_array()?;
        if cells.len() != columns.len() {
            return None;
        }
        rows.push(
            cells
                .iter()
                .map(Value::from_json)
                .collect::<Option<Vec<_>>>()?,
        );
    }
    let rows_affected = v.get("rows_affected")?.as_u64()?;
    Some(QueryResult {
        columns,
        rows,
        rows_affected,
    })
}

/// Client error carrying the gateway's stable error code when one was
/// returned, or a transport-side code (`transport_unavailable`,
/// `invalid_gateway_response`, `outcome_unknown`, `invalid_server`,
/// `invalid_name`, `invalid_argument`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientError {
    pub code: String,
    pub message: String,
    pub status: Option<u16>,
}

impl ClientError {
    fn new(code: &str, message: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            status,
        }
    }
    fn invalid_gateway(detail: &str) -> Self {
        Self::new(
            "invalid_gateway_response",
            format!("{detail}; the gateway response was not the expected shape"),
            Some(502),
        )
    }
    fn outcome_unknown(detail: &str) -> Self {
        Self::new(
            "outcome_unknown",
            format!(
                "{detail}; a write may have committed — replay only with the same idempotency key within its retention window, never blindly retry an unkeyed write"
            ),
            Some(502),
        )
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ClientError {}

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Clone, Debug, Deserialize)]
pub struct Status {
    pub version: String,
    pub uptime_secs: u64,
    pub admission: serde_json::Value,
    pub requests: serde_json::Value,
    #[serde(default)]
    pub databases: Option<serde_json::Value>,
    /// Where the gateway is running, and where it keeps its files.
    ///
    /// `None` against a gateway that does not report its placement: the
    /// question is then unanswerable, not answered with blanks.
    #[serde(default)]
    pub deployment: Option<Deployment>,
}

/// Where the gateway is running, and where each of its databases is.
#[derive(Clone, Debug, Deserialize)]
pub struct Deployment {
    /// The machine's hostname, when the platform reports one.
    pub host: Option<String>,
    /// The address the listener actually bound.
    pub listen: String,
    /// The directory the gateway keeps its databases in.
    pub data_dir: String,
    /// The backup root the gateway watches, when one is configured.
    pub backup_root: Option<String>,
    /// Where each registered database lives.
    #[serde(default)]
    pub databases: std::collections::BTreeMap<String, DatabaseLocation>,
    /// The free space on the volume the data directory lives on, when the
    /// platform reports it. The server publishes it so an operator can see
    /// the pressure before a write fails.
    #[serde(default)]
    pub disk: Option<DiskSpace>,
    /// The total the database files occupy, in bytes.
    #[serde(default)]
    pub databases_bytes: Option<u64>,
    /// How long a deleted name is held before it is purged, in seconds.
    #[serde(default)]
    pub retention_secs: Option<u64>,
}

/// The free space on the volume the gateway keeps its files on.
#[derive(Clone, Debug, Deserialize)]
pub struct DiskSpace {
    /// The volume's total size, in bytes.
    #[serde(default)]
    pub total_bytes: Option<u64>,
    /// Bytes still available on the volume.
    #[serde(default)]
    pub free_bytes: Option<u64>,
    /// Bytes in use on the volume.
    #[serde(default)]
    pub used_bytes: Option<u64>,
    /// How full the volume is, as a percentage.
    #[serde(default)]
    pub used_percent: Option<f64>,
    /// The volume's total inode count.
    #[serde(default)]
    pub total_inodes: Option<u64>,
    /// The volume's free inode count.
    #[serde(default)]
    pub free_inodes: Option<u64>,
}

/// Where one database's file is, and whether it is there.
#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseLocation {
    /// The absolute path of the file that database lives in.
    pub file: String,
    /// Whether that file is there now.
    pub exists: bool,
    /// The main file plus its WAL sidecars, in bytes.
    pub size_bytes: u64,
    /// Whether the name is tombstoned: deleted, but still on disk until
    /// the retention window purges it.
    pub tombstoned: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TableInfo {
    pub name: String,
    pub rows: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseInfo {
    pub name: String,
    pub tombstoned: bool,
    pub size_bytes: u64,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub purge_after: Option<u64>,
    #[serde(default)]
    pub tables: Vec<TableInfo>,
}

/// A connection to one gateway origin with one credential (owner or a
/// database-scoped key). Cheap to clone; the HTTP pool is shared.
///
/// The [`std::fmt::Debug`] implementation is hand-written: the derived
/// one printed the credential in full, and a derived `Debug` is a promise
/// that the type's debug output is safe to log. The credential is the
/// one value in this type that must never reach a log.
#[derive(Clone)]
pub struct Gateway {
    origin: String,
    credential: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Gateway")
            .field("origin", &self.origin)
            .field("credential", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// Validate the origin and build the client. Refuses non-loopback
    /// HTTP, userinfo, query/fragment, and non-root paths — the same
    /// rule the TS client and CLI enforce.
    pub fn new(server: &str, credential: &str) -> Result<Self> {
        let url = reqwest::Url::parse(server)
            .map_err(|_| ClientError::new("invalid_server", "server is not a valid URL", None))?;
        let loopback = url
            .host_str()
            .map(|h| {
                h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(ClientError::new(
                "invalid_server",
                "use HTTPS or a literal loopback HTTP origin",
                None,
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| {
                ClientError::new("transport_unavailable", "cannot build HTTP client", None)
            })?;
        Ok(Self {
            origin: server.trim_end_matches('/').to_owned(),
            credential: credential.to_owned(),
            client,
        })
    }

    /// Build from a 0600 credential file (owner or scoped key), the way
    /// applications on the host receive their key.
    pub fn from_key_file(server: &str, path: &std::path::Path) -> Result<Self> {
        let key = read_key_file(path)?;
        Self::new(server, &key)
    }

    pub fn database(&self, name: &str) -> Result<Database> {
        if !valid_name(name) {
            return Err(ClientError::new(
                "invalid_name",
                format!("invalid database name: {name}"),
                None,
            ));
        }
        Ok(Database {
            gateway: self.clone(),
            name: name.to_owned(),
        })
    }

    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let v = self
            .request(
                reqwest::Method::GET,
                "/v1/databases",
                None,
                None,
                Semantics::Read,
            )
            .await?;
        let names = v
            .get("databases")
            .and_then(Json::as_array)
            .and_then(|a| {
                a.iter()
                    .map(|n| n.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| ClientError::invalid_gateway("malformed list response"))?;
        Ok(names)
    }

    pub async fn status(&self) -> Result<Status> {
        let v = self
            .request(
                reqwest::Method::GET,
                "/v1/status",
                None,
                None,
                Semantics::Read,
            )
            .await?;
        serde_json::from_value(v)
            .map_err(|_| ClientError::invalid_gateway("malformed status response"))
    }

    pub async fn database_info(&self, name: &str) -> Result<DatabaseInfo> {
        if !valid_name(name) {
            return Err(ClientError::new(
                "invalid_name",
                format!("invalid database name: {name}"),
                None,
            ));
        }
        let v = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/databases/{name}"),
                None,
                None,
                Semantics::Read,
            )
            .await?;
        serde_json::from_value(v)
            .map_err(|_| ClientError::invalid_gateway("malformed info response"))
    }

    /// Owner-only: create an empty database.
    pub async fn create_database(&self, name: &str) -> Result<()> {
        if !valid_name(name) {
            return Err(ClientError::new(
                "invalid_name",
                format!("invalid database name: {name}"),
                None,
            ));
        }
        let v = self
            .request(
                reqwest::Method::POST,
                "/v1/databases",
                Some(json!({ "name": name })),
                None,
                Semantics::Write,
            )
            .await?;
        if v.get("name").and_then(Json::as_str) != Some(name) {
            return Err(ClientError::outcome_unknown("malformed create response"));
        }
        Ok(())
    }

    /// Owner-only: tombstone a database (purge follows the gateway's
    /// retention). Returns the purge delay in seconds.
    pub async fn delete_database(&self, name: &str) -> Result<u64> {
        if !valid_name(name) {
            return Err(ClientError::new(
                "invalid_name",
                format!("invalid database name: {name}"),
                None,
            ));
        }
        let v = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v1/databases/{name}"),
                None,
                None,
                Semantics::Write,
            )
            .await?;
        if v.get("deleted").and_then(Json::as_bool) != Some(true) {
            return Err(ClientError::outcome_unknown("malformed delete response"));
        }
        v.get("purge_after_seconds")
            .and_then(Json::as_u64)
            .ok_or_else(|| ClientError::outcome_unknown("malformed delete response"))
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Json>,
        idempotency: Option<&str>,
        semantics: Semantics,
    ) -> Result<Json> {
        let may_write = semantics == Semantics::Write;
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.origin))
            .bearer_auth(&self.credential)
            // A read that is given up on costs a retry and nothing else,
            // so it gives up quickly; a write that is given up on leaves
            // the outcome in doubt, so it waits far longer — the answer
            // is what removes the doubt. The CLI and the TypeScript
            // client follow the same rule.
            .timeout(if may_write {
                WRITE_TIMEOUT
            } else {
                READ_TIMEOUT
            });
        if let Some(body) = body {
            req = req.json(&body);
        }
        if let Some(key) = idempotency {
            req = req.header("Idempotency-Key", key);
        }
        let mut response = req.send().await.map_err(|_| {
            if may_write {
                ClientError::outcome_unknown("transport failed")
            } else {
                ClientError::new(
                    "transport_unavailable",
                    "transport failed; the gateway is unavailable",
                    Some(503),
                )
            }
        })?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            if may_write {
                ClientError::outcome_unknown("response lost")
            } else {
                ClientError::invalid_gateway("response interrupted")
            }
        })? {
            if bytes.len() + chunk.len() > DEFAULT_MAX_RESPONSE {
                return Err(if may_write {
                    ClientError::outcome_unknown("response too large")
                } else {
                    ClientError::invalid_gateway("response too large")
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Json = serde_json::from_slice(&bytes).map_err(|_| {
            if may_write {
                ClientError::outcome_unknown("invalid response")
            } else {
                ClientError::invalid_gateway("invalid JSON")
            }
        })?;
        if !status.is_success() {
            if let Some(err) = value.get("error") {
                let code = err
                    .get("code")
                    .and_then(Json::as_str)
                    .unwrap_or("http_error")
                    .to_owned();
                let message = err
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("gateway error")
                    .to_owned();
                return Err(ClientError::new(&code, message, Some(status.as_u16())));
            }
            return Err(if may_write {
                ClientError::outcome_unknown("malformed error response")
            } else {
                ClientError::invalid_gateway("malformed error response")
            });
        }
        Ok(value)
    }
}

/// One database, queried through a scoped (or owner) credential.
#[derive(Clone, Debug)]
pub struct Database {
    gateway: Gateway,
    name: String,
}

impl Database {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Run one statement. Unkeyed: a transport failure leaves the
    /// outcome unknown for writes — prefer [`Self::execute_with_key`]
    /// for writes that matter.
    pub async fn execute(&self, sql: &str, args: Vec<Value>) -> Result<QueryResult> {
        self.execute_inner(sql, args, None).await
    }

    /// Run one statement with an idempotency key: replaying with the
    /// same key/body inside the retention window returns the original
    /// result instead of re-applying the write.
    pub async fn execute_with_key(
        &self,
        sql: &str,
        args: Vec<Value>,
        idempotency_key: &str,
    ) -> Result<QueryResult> {
        if idempotency_key.is_empty() {
            return Err(ClientError::new(
                "invalid_argument",
                "idempotency key must not be empty",
                None,
            ));
        }
        self.execute_inner(sql, args, Some(idempotency_key)).await
    }

    async fn execute_inner(
        &self,
        sql: &str,
        args: Vec<Value>,
        idempotency: Option<&str>,
    ) -> Result<QueryResult> {
        if sql.is_empty() {
            return Err(ClientError::new(
                "invalid_argument",
                "SQL must not be empty",
                None,
            ));
        }
        let body = json!({
            "sql": sql,
            "args": args.iter().map(Value::to_json).collect::<Vec<_>>(),
        });
        // The statements decide whether this is a read or a write: a
        // query is a POST either way, so the method says nothing, and a
        // read that is given up on must not be reported as a write that
        // may have committed.
        let semantics = semantics_of(sql);
        let v = self
            .gateway
            .request(
                reqwest::Method::POST,
                &format!("/v1/databases/{}/query", self.name),
                Some(body),
                idempotency,
                semantics,
            )
            .await?;
        parse_result(&v).ok_or_else(|| match semantics {
            Semantics::Read => ClientError::invalid_gateway("malformed query response"),
            Semantics::Write => ClientError::outcome_unknown("malformed query response"),
        })
    }

    /// Run an atomic batch. Keyed replay works as with `execute_with_key`.
    pub async fn batch(&self, statements: Vec<Statement>) -> Result<Vec<QueryResult>> {
        self.batch_inner(statements, None).await
    }

    pub async fn batch_with_key(
        &self,
        statements: Vec<Statement>,
        idempotency_key: &str,
    ) -> Result<Vec<QueryResult>> {
        if idempotency_key.is_empty() {
            return Err(ClientError::new(
                "invalid_argument",
                "idempotency key must not be empty",
                None,
            ));
        }
        self.batch_inner(statements, Some(idempotency_key)).await
    }

    async fn batch_inner(
        &self,
        statements: Vec<Statement>,
        idempotency: Option<&str>,
    ) -> Result<Vec<QueryResult>> {
        if statements.is_empty() || statements.iter().any(|s| s.sql.is_empty()) {
            return Err(ClientError::new(
                "invalid_argument",
                "batch needs at least one non-empty statement",
                None,
            ));
        }
        let body = json!({
            "statements": statements.iter().map(|s| json!({
                "sql": s.sql,
                "args": s.args.iter().map(Value::to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "atomic": true,
        });
        // A batch is a read only when every statement in it is one.
        let semantics = semantics_of_statements(&statements);
        let v = self
            .gateway
            .request(
                reqwest::Method::POST,
                &format!("/v1/databases/{}/batch", self.name),
                Some(body),
                idempotency,
                semantics,
            )
            .await?;
        let results = v
            .get("results")
            .and_then(Json::as_array)
            .and_then(|a| a.iter().map(parse_result).collect::<Option<Vec<_>>>())
            .ok_or_else(|| match semantics {
                Semantics::Read => ClientError::invalid_gateway("malformed batch response"),
                Semantics::Write => ClientError::outcome_unknown("malformed batch response"),
            })?;
        Ok(results)
    }

    pub async fn info(&self) -> Result<DatabaseInfo> {
        self.gateway.database_info(&self.name).await
    }
}

/// Read a 64-hex credential from a file, refusing group/other access on
/// Unix — the same rule as the CLI's key files.
pub fn read_key_file(path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(path)
        .map_err(|_| ClientError::new("missing_credential", "credential file unreadable", None))?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(ClientError::new(
            "missing_credential",
            "credential file has an unexpected shape",
            None,
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ClientError::new(
                "insecure_key_file",
                "credential file must not be accessible to group or others",
                None,
            ));
        }
    }
    let key = std::fs::read_to_string(path)
        .map_err(|_| ClientError::new("missing_credential", "credential file unreadable", None))?;
    let key = key.trim();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ClientError::new(
            "missing_credential",
            "credential file does not hold a 64-hex key",
            None,
        ));
    }
    Ok(key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules_match_the_other_clients() {
        assert!(Gateway::new("http://127.0.0.1:7432", "k").is_ok());
        assert!(Gateway::new("http://[::1]:7432", "k").is_ok());
        assert!(Gateway::new("https://gw.example.com", "k").is_ok());
        // Plain HTTP off loopback, userinfo, query, and paths all refuse.
        assert_eq!(
            Gateway::new("http://gw.example.com", "k").unwrap_err().code,
            "invalid_server"
        );
        assert!(Gateway::new("http://user@127.0.0.1:7432", "k").is_err());
        assert!(Gateway::new("http://127.0.0.1:7432/?x=1", "k").is_err());
        assert!(Gateway::new("http://127.0.0.1:7432/v1", "k").is_err());
        assert!(Gateway::new("http://localhost:7432", "k").is_err());
    }

    #[test]
    fn names_follow_the_gateway_rule() {
        assert!(valid_name("billing_dev"));
        assert!(valid_name("a"));
        assert!(!valid_name(""));
        assert!(!valid_name("Billing"));
        assert!(!valid_name("_lead"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn values_round_trip_the_gateway_encoding() {
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::Integer(-42),
            Value::Real(1.5),
            Value::Text("héllo".into()),
            Value::Blob(vec![0, 1, 255]),
        ] {
            assert_eq!(Value::from_json(&v.to_json()), Some(v));
        }
        // Objects have no SQL value encoding and must not parse.
        assert_eq!(Value::from_json(&json!({"a": 1})), None);
        // A byte array with an out-of-range member is not a blob.
        assert_eq!(Value::from_json(&json!([256])), None);
    }

    #[test]
    fn the_statements_decide_whether_a_request_is_a_read() {
        // The rule the server uses, so the two agree: a statement that
        // opens with SELECT/VALUES/EXPLAIN, or with WITH and holds no
        // data-modifying keyword, is a read.
        for sql in [
            "SELECT 1",
            "select * from notes where id = ?1",
            "VALUES (1), (2)",
            "EXPLAIN QUERY PLAN SELECT 1",
            "WITH recent AS (SELECT id FROM notes) SELECT * FROM recent",
            "  SELECT 1",
        ] {
            assert_eq!(semantics_of(sql), Semantics::Read, "{sql:?} is a read");
        }
        for sql in [
            "INSERT INTO notes VALUES (1)",
            "UPDATE notes SET body = 'x'",
            "DELETE FROM notes",
            "CREATE TABLE notes (id INTEGER)",
            "DROP TABLE notes",
            "ALTER TABLE notes ADD COLUMN body TEXT",
            "REPLACE INTO notes VALUES (1)",
            "VACUUM",
            "REINDEX notes",
            "WITH moved AS (SELECT id FROM notes) INSERT INTO notes SELECT id FROM moved",
            "PRAGMA journal_mode",
            "BEGIN",
        ] {
            assert_eq!(semantics_of(sql), Semantics::Write, "{sql:?} is a write");
        }
    }

    #[test]
    fn a_batch_is_a_read_only_when_every_statement_is_one() {
        let reads = vec![Statement::new("SELECT 1"), Statement::new("SELECT 2")];
        assert_eq!(semantics_of_statements(&reads), Semantics::Read);
        let mixed = vec![
            Statement::new("SELECT 1"),
            Statement::new("DELETE FROM notes"),
        ];
        assert_eq!(semantics_of_statements(&mixed), Semantics::Write);
        assert_eq!(semantics_of_statements(&[]), Semantics::Read);
    }

    #[test]
    fn result_parsing_checks_row_width() {
        let good = json!({"columns": ["a"], "rows": [[1], ["x"]], "rows_affected": 0});
        let parsed = parse_result(&good).expect("valid result");
        assert_eq!(parsed.rows.len(), 2);
        let bad = json!({"columns": ["a", "b"], "rows": [[1]], "rows_affected": 0});
        assert_eq!(parse_result(&bad), None);
    }
}
