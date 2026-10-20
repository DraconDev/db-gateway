//! Everyday data commands against the API: list/info/create,
//! query/batch, and the interactive shell.

use super::{Request, api_error, api_failed};
use dg::ServerError::Application;
use dg::error::Error;
use dg::render::{Align, Cell, Section, Style, Table, Theme};
use reqwest::Method;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{IsTerminal, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    // Interactive terminals get a table; piped output and --json stay
    // machine-readable JSON.
    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let result = dg::client::request(server, &key, Method::GET, "/v1/databases", None, None)
            .await
            .map_err(Application)?;
        if api_failed(&result) {
            dg::shell::render_with(&result, false, &terminal_theme(), &mut std::io::stdout())
                .map_err(Application)?;
            return Err(Application(api_error()));
        }
        let names: Vec<String> = result["databases"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|name| name.as_str().unwrap_or("").to_string())
            .collect();
        let mut table = Table::new();
        table.column("database", Align::Left);
        for name in &names {
            table.row(vec![Cell::styled(name.clone(), Style::Cyan)]);
        }
        table.footer(if names.len() == 1 {
            "1 database".to_string()
        } else {
            format!("{} databases", names.len())
        });
        let mut out = std::io::stdout();
        table
            .write(&mut out, &terminal_theme())
            .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/databases".into(), None, None)))
}

/// The per-database live view: what this database holds and what it
/// has been doing.
///
/// `dg info` is what a database IS; `dg inspect` is what it has been
/// DOING. It reads the same facts the service dashboard shows, but for
/// one database, so an operator can look at the one that is busy
/// without reading the whole list.
pub async fn inspect(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let info = dg::client::request(
            server,
            &key,
            Method::GET,
            &format!("/v1/databases/{name}"),
            None,
            None,
        )
        .await
        .map_err(Application)?;
        if api_failed(&info) {
            dg::shell::render_with(&info, false, &terminal_theme(), &mut std::io::stdout())
                .map_err(Application)?;
            return Err(Application(api_error()));
        }
        // The usage and backup facts are auxiliary: a failed fetch
        // degrades those sections instead of failing the command.
        let status = dg::client::request(server, &key, Method::GET, "/v1/status", None, None)
            .await
            .ok()
            .filter(|status| !api_failed(status));
        let backups = dg::client::request(server, &key, Method::GET, "/v1/backups", None, None)
            .await
            .ok()
            .filter(|backups| !api_failed(backups));

        let mut facts = Section::new("database");
        facts.field("name", Cell::styled(&name, Style::Cyan));
        facts.field(
            "size",
            Cell::text(
                info["size_bytes"]
                    .as_u64()
                    .map(dg::render::human_bytes)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        facts.field(
            "state",
            if info["tombstoned"].as_bool() == Some(true) {
                Cell::styled("tombstoned", Style::Yellow)
            } else {
                Cell::styled("live", Style::Green)
            },
        );
        if info["tombstoned"].as_bool() == Some(true) {
            facts.field("purge after", Cell::text(purge_after_text(&info)));
        } else {
            facts.field("created", Cell::text(created_text(&info)));
        }
        let mut sections = vec![facts];

        // What this database has been doing.
        let mut activity = Section::new("activity");
        let entry = status
            .as_ref()
            .and_then(|status| status["databases"][&name].as_object().cloned())
            .unwrap_or_default();
        activity.field(
            "rows read",
            Cell::number(entry.get("rows_read").and_then(Value::as_u64).unwrap_or(0)),
        );
        activity.field(
            "rows written",
            Cell::number(
                entry
                    .get("rows_written")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
        );
        let waits = entry
            .get("queue_waits")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        activity.field("queue waits", Cell::number(waits));
        activity.field(
            "wait p95",
            match (
                waits,
                entry.get("queue_wait_max_us").and_then(Value::as_u64),
            ) {
                (0, _) => Cell::text("no waits recorded"),
                (_, Some(max)) => Cell::text(dg::render::human_micros(max)),
                (_, None) => Cell::text("-"),
            },
        );
        activity.field(
            "last write",
            match entry.get("last_write").and_then(Value::as_u64) {
                Some(epoch) => Cell::text(last_write_ago(epoch)),
                None => Cell::text("never"),
            },
        );
        sections.push(activity);

        // How its backup is doing.
        let mut backup = Section::new("backup");
        let health = backups
            .as_ref()
            .and_then(|backups| backups["per_database"][&name].as_object().cloned())
            .unwrap_or_default();
        let state = health
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        backup.field("state", Cell::styled(state, backup_state_style(state)));
        backup.field(
            "newest set",
            Cell::text(
                health
                    .get("newest_set")
                    .and_then(Value::as_str)
                    .map(dg::render::human_set_stamp)
                    .unwrap_or_else(|| "-".to_string()),
            ),
        );
        backup.field(
            "age",
            Cell::text(
                health
                    .get("age_secs")
                    .and_then(Value::as_u64)
                    .map(dg::render::human_duration)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        sections.push(backup);

        // What it holds.
        if info["tombstoned"].as_bool() != Some(true) {
            let mut tables_section = Section::new("tables");
            match info["tables"].as_array() {
                Some(entries) if !entries.is_empty() => {
                    let mut table = Table::new();
                    table.column("table", Align::Left);
                    table.column("rows", Align::Right);
                    for entry in entries {
                        table.row(vec![
                            Cell::text(entry["name"].as_str().unwrap_or("?")),
                            Cell::number(entry["rows"].as_u64().unwrap_or(0)),
                        ]);
                    }
                    tables_section.table(table);
                }
                _ => {
                    tables_section.field("none", Cell::text("this database has no tables"));
                }
            }
            sections.push(tables_section);
        }
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        format!("/v1/databases/{name}"),
        None,
        None,
    )))
}

/// The freshness of a backup, as a colour that means something.
fn backup_state_style(state: &str) -> Style {
    match state {
        "fresh" => Style::Green,
        "stale" => Style::Yellow,
        _ => Style::Red,
    }
}

/// How long ago a write happened, as a person reads it.
fn last_write_ago(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch);
    format!(
        "{} ago",
        dg::render::human_duration(now.saturating_sub(epoch))
    )
}

pub async fn info(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let info = dg::client::request(
            server,
            &key,
            Method::GET,
            &format!("/v1/databases/{name}"),
            None,
            None,
        )
        .await
        .map_err(Application)?;
        if api_failed(&info) {
            dg::shell::render_with(&info, false, &terminal_theme(), &mut std::io::stdout())
                .map_err(Application)?;
            return Err(Application(api_error()));
        }
        // Usage is auxiliary: a failed status fetch degrades the
        // two usage cells instead of failing the command. Absent
        // from a good status means true zero activity.
        let (rows_read, rows_written) =
            dg::client::request(server, &key, Method::GET, "/v1/status", None, None)
                .await
                .ok()
                .filter(|status| !api_failed(status))
                .map(|status| {
                    let use_ = &status["databases"][&name];
                    (
                        use_["rows_read"].as_u64().unwrap_or(0).to_string(),
                        use_["rows_written"].as_u64().unwrap_or(0).to_string(),
                    )
                })
                .unwrap_or_else(|| ("unavailable".to_string(), "unavailable".to_string()));

        // A dashboard, not a flat property list: what the database is,
        // then what it holds, then how it has been used.
        let mut facts = Section::new("database");
        facts.field(
            "name",
            Cell::styled(info["name"].as_str().unwrap_or(""), Style::Cyan),
        );
        facts.field(
            "tombstoned",
            if info["tombstoned"].as_bool() == Some(true) {
                Cell::styled("yes", Style::Yellow)
            } else {
                Cell::text("no")
            },
        );
        facts.field(
            "size",
            // A humanised size is text, not a number: "16.0 KiB" is a
            // quantity with a unit, and labelling it a number would
            // promise a right-aligned figure it is not.
            Cell::text(
                info["size_bytes"]
                    .as_u64()
                    .map(dg::render::human_bytes)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        if info["tombstoned"].as_bool() == Some(true) {
            facts.field("purge after", Cell::text(purge_after_text(&info)));
        } else {
            facts.field("created", Cell::text(created_text(&info)));
        }

        let mut usage = Section::new("usage");
        usage.field("rows read", Cell::number(rows_read));
        usage.field("rows written", Cell::number(rows_written));

        let mut sections = vec![facts, usage];
        if info["tombstoned"].as_bool() != Some(true) {
            let mut tables_section = Section::new("tables");
            match info["tables"].as_array() {
                Some(entries) if !entries.is_empty() => {
                    let mut table = Table::new();
                    table.column("table", Align::Left);
                    table.column("rows", Align::Right);
                    for entry in entries {
                        table.row(vec![
                            Cell::text(entry["name"].as_str().unwrap_or("?")),
                            Cell::number(entry["rows"].as_u64().unwrap_or(0)),
                        ]);
                    }
                    tables_section.table(table);
                }
                _ => {
                    tables_section.field("none", Cell::text("this database has no tables"));
                }
            }
            sections.push(tables_section);
        }
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        format!("/v1/databases/{name}"),
        None,
        None,
    )))
}

/// The creation time as a person reads it, with the raw value kept
/// alongside it: the human form is for reading, the epoch is for
/// comparing.
fn created_text(info: &Value) -> String {
    match info["created_at"].as_i64() {
        Some(epoch) => format!("{} ({epoch})", dg::render::human_epoch(epoch)),
        None => "unknown".to_string(),
    }
}

/// The purge time as a person reads it.
fn purge_after_text(info: &Value) -> String {
    match info["purge_after"].as_i64() {
        Some(epoch) => format!("{} ({epoch})", dg::render::human_epoch(epoch)),
        None => "unknown".to_string(),
    }
}

pub async fn create(name: String) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    Ok(Some((
        Method::POST,
        "/v1/databases".into(),
        Some(json!({"name":name})),
        None,
    )))
}

pub async fn shell(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: Option<String>,
) -> Result<(), dg::ServerError> {
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    if let Some(name) = &name {
        dg::store::validate_name(name).map_err(Application)?;
    }
    return dg::shell::run(
        server,
        &key,
        json,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        name.as_deref(),
        std::io::stdin(),
        std::io::stdout(),
    )
    .await
    .map_err(Application);
}

/// Run a SQL file against a database, reporting each statement.
///
/// A script is split into statements by SQLite itself, so a semicolon
/// inside a string or a trigger body does not end a statement. The
/// statements run as ONE atomic batch: either every statement lands or
/// none does, which is what a migration script means.
pub async fn execute(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    file: PathBuf,
    idempotency_key: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let script = std::fs::read_to_string(&file)
        .map_err(|_| Application(Error::new(400, "unreadable_file", "cannot read the SQL file")))?;
    let statements = dg::sqlsplit::split_statements(&script);
    if statements.is_empty() {
        return Err(Application(Error::new(
            400,
            "empty_script",
            "the SQL file holds no statements",
        )));
    }
    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let body = json!({
            "statements": statements.iter().map(|sql| json!({"sql": sql, "args": []})).collect::<Vec<_>>(),
            "atomic": true,
        });
        let result = dg::client::request(
            server,
            &key,
            Method::POST,
            &format!("/v1/databases/{name}/batch"),
            Some(body),
            idempotency_key.as_deref(),
        )
        .await
        .map_err(Application)?;
        // The per-statement outcomes are the point of running a script,
        // so they are drawn rather than dumped.
        let mut out = std::io::stdout();
        let theme = terminal_theme();
        let mut table = Table::new();
        table.column("#", Align::Right);
        table.column("statement", Align::Left);
        table.column("result", Align::Right);
        let results = result["results"].as_array().cloned().unwrap_or_default();
        for (index, statement) in statements.iter().enumerate() {
            let outcome = match results.get(index) {
                Some(one) => match one["error"].as_str() {
                    Some(message) => Cell::styled(
                        message.to_string(),
                        dg::render::Style::Red,
                    ),
                    None => match one["rows_affected"].as_u64() {
                        Some(rows) => Cell::text(format!("{rows} rows")),
                        None => {
                            let rows = one["rows"].as_array().map(|rows| rows.len()).unwrap_or(0);
                            Cell::text(format!("{rows} rows"))
                        }
                    },
                },
                None => Cell::text("-"),
            };
            table.row(vec![
                Cell::number(index + 1),
                Cell::text(statement),
                outcome,
            ]);
        }
        table
            .write(&mut out, &theme)
            .map_err(Application)?;
        if result.get("error").is_some() {
            return Err(Application(Error::new(
                400,
                "api_error",
                "API request failed; see response above",
            )));
        }
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/batch"),
        Some(json!({
            "statements": statements.iter().map(|sql| json!({"sql": sql, "args": []})).collect::<Vec<_>>(),
            "atomic": true,
        })),
        idempotency_key,
    )))
}

pub async fn query(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    sql: String,
    args: String,
    idempotency_key: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let args: Vec<Value> =
        serde_json::from_str(&args).map_err(|_| Application(Error::invalid()))?;
    // Interactive terminals get the shell's aligned table; piped
    // output and --json stay machine-readable JSON.
    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let result = dg::client::request(
            server,
            &key,
            Method::POST,
            &format!("/v1/databases/{name}/query"),
            Some(json!({"sql":sql,"args":args})),
            idempotency_key.as_deref(),
        )
        .await
        .map_err(Application)?;
        // The theme comes from the real stdout, so the table is drawn
        // for a terminal: the same grid the shell draws.
        dg::shell::render_with(&result, false, &terminal_theme(), &mut std::io::stdout())
            .map_err(Application)?;
        if result.get("error").is_some() {
            return Err(Application(Error::new(
                400,
                "api_error",
                "API request failed; see response above",
            )));
        }
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/query"),
        Some(json!({"sql":sql,"args":args})),
        idempotency_key,
    )))
}

pub async fn batch(
    name: String,
    file: PathBuf,
    idempotency_key: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    // Open once with O_NONBLOCK so a writer-less FIFO cannot make
    // the CLI hang before the descriptor is checked. The metadata
    // check is on that same descriptor, so a path replacement
    // cannot turn this into a read of a different file.
    #[cfg(unix)]
    let batch_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&file);
    #[cfg(not(unix))]
    let batch_file = OpenOptions::new().read(true).open(&file);
    let mut batch_file = batch_file.map_err(|_| Application(Error::invalid()))?;
    if !batch_file
        .metadata()
        .map_err(|_| Application(Error::invalid()))?
        .is_file()
    {
        return Err(Application(Error::invalid()));
    }
    let mut batch_bytes = Vec::new();
    Read::by_ref(&mut batch_file)
        .take(65537)
        .read_to_end(&mut batch_bytes)
        .map_err(|_| Application(Error::invalid()))?;
    if batch_bytes.len() > 65536 {
        return Err(Application(Error::new(
            413,
            "input_limit_exceeded",
            "batch input exceeds 64 KiB",
        )));
    }
    let body: dg::execution::Batch =
        serde_json::from_slice(&batch_bytes).map_err(|_| Application(Error::invalid()))?;
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/batch"),
        Some(serde_json::to_value(body).map_err(|_| Application(Error::invalid()))?),
        idempotency_key,
    )))
}
