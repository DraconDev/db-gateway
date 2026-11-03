//! `dg audit`: which credential ran what, and when.
//!
//! The audit log is the answer to the question an operator asks after
//! something unexpected: which credential did this? The gateway records
//! the ACTIONS -- a credential ran a statement, created a key, deleted
//! a database -- and never the data that moved, so the log says which
//! statement ran without becoming a second copy of the database.
//!
//! A terminal gets the table; a pipe or `--json` gets the API's own
//! JSON, unchanged, so a script reads exactly what the endpoint says.

use std::io::IsTerminal;
use std::path::PathBuf;

use dg::client::Semantics;
use dg::error::Error;
use dg::render::{Align, Cell, Outcome, Style, Table, Theme};

/// A request the shared tail in `main` issues on behalf of a command:
/// the method, the path, the body, the idempotency key, and the
/// transport semantics.
///
/// The same shape the other commands use, so the audit command rides
/// the same tail: the JSON it prints is the API's document, unchanged,
/// and an API error is reported the way every other command reports
/// one.
pub type Request = (
    reqwest::Method,
    String,
    Option<serde_json::Value>,
    Option<String>,
    Semantics,
);

/// The recorded actions.
///
/// The filters are the endpoint's own: a credential's label, a Unix
/// second, and how many of the newest entries to keep.
pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    credential: Option<&str>,
    since: Option<u64>,
    limit: Option<usize>,
) -> Result<Option<Request>, dg::ServerError> {
    let path = path(credential, since, limit);
    if json || !std::io::stdout().is_terminal() {
        // The JSON is the API's document, unchanged: a pipe reads the
        // same thing a terminal does, minus the drawing.
        return Ok(Some((
            reqwest::Method::GET,
            path,
            None,
            None,
            Semantics::Read,
        )));
    }
    let key = dg::client::read_key(key_path.as_deref().ok_or_else(|| {
        dg::ServerError::Application(Error::new(400, "missing_credential", "--key-file required"))
    })?)
    .map_err(dg::ServerError::Application)?;
    let result = dg::client::request_with_semantics(
        server,
        &key,
        reqwest::Method::GET,
        &path,
        None,
        None,
        Semantics::Read,
    )
    .await
    .map_err(dg::ServerError::Application)?;
    if result.get("error").is_some() {
        dg::shell::render_with(&result, false, &theme(), &mut std::io::stdout())
            .map_err(dg::ServerError::Application)?;
        return Err(dg::ServerError::Application(Error::new(
            400,
            "api_error",
            "API request failed; see JSON response",
        )));
    }
    let mut out = std::io::stdout();
    Outcome::list("audit", table(&result))
        .write(&mut out, &theme())
        .map_err(dg::ServerError::Application)?;
    Ok(None)
}

/// The theme for a command that is drawing to the terminal.
fn theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// The endpoint path, with the filters the endpoint takes.
fn path(credential: Option<&str>, since: Option<u64>, limit: Option<usize>) -> String {
    let mut query: Vec<String> = Vec::new();
    if let Some(name) = credential {
        query.push(format!("principal={}", encode(name)));
    }
    if let Some(since) = since {
        query.push(format!("since={since}"));
    }
    if let Some(limit) = limit {
        query.push(format!("limit={limit}"));
    }
    if query.is_empty() {
        "/v1/audit".to_string()
    } else {
        format!("/v1/audit?{}", query.join("&"))
    }
}

/// Percent-encode a query value.
///
/// A credential's name is a plain word in practice, but the value ends
/// up in a URL, so it is encoded rather than trusted: a name carrying
/// an `&` must not be able to add a filter the operator did not ask
/// for.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The recorded actions as a table: when, who, what, how long.
///
/// The times are the ones a person reads, not the epoch seconds the
/// API carries.
pub fn table(payload: &serde_json::Value) -> Table {
    let mut table = Table::new();
    table.column("time", Align::Left);
    table.column("credential", Align::Left);
    table.column("role", Align::Left);
    table.column("action", Align::Left);
    table.column("elapsed", Align::Left);
    table.column("database", Align::Left);
    table.column("detail", Align::Left);
    let entries = payload["entries"].as_array().cloned().unwrap_or_default();
    for entry in &entries {
        table.row(vec![
            Cell::styled(human_time(entry), Style::Dim),
            Cell::styled(field(entry, "principal"), Style::Cyan),
            Cell::text(field(entry, "role")),
            Cell::text(field(entry, "action")),
            Cell::text(human_elapsed(entry)),
            Cell::text(field(entry, "database")),
            Cell::text(field(entry, "detail")),
        ]);
    }
    table.footer(format!("{} entries", entries.len()));
    table
}

/// When an entry happened, as a person reads it.
fn human_time(entry: &serde_json::Value) -> String {
    let at = entry
        .get("at")
        .and_then(|value| value.as_u64())
        .unwrap_or_default();
    dg::render::human_epoch(i64::try_from(at).unwrap_or(i64::MAX))
}

/// How long the entry's statement took, as a person reads it.
///
/// An entry that is not a statement has no time, and a trail line
/// from before the field existed has none either: both render empty
/// rather than a word, like any other missing field.
fn human_elapsed(entry: &serde_json::Value) -> String {
    entry
        .get("elapsed_us")
        .and_then(|value| value.as_u64())
        .map(dg::render::human_micros)
        .unwrap_or_default()
}

/// One field of an entry, as text.
///
/// A field that is missing is empty rather than a word: the log says
/// what it knows, and an action with no database is an action with no
/// database.
fn field(entry: &serde_json::Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}
