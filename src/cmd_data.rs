//! Everyday data commands against the API: list/info/create,
//! query/batch, and the interactive shell.

use super::{Request, api_error, api_failed};
use dg::render::{Align, Cell, Section, Style, Table, Theme};
use dg::ServerError::Application;
use dg::error::Error;
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
            dg::shell::render(&result, false, &mut std::io::stdout()).map_err(Application)?;
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
        table.footer(format!("{} database(s)", names.len()));
        let mut out = std::io::stdout();
        table.write(&mut out, &terminal_theme()).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/databases".into(), None, None)))
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
            dg::shell::render(&info, false, &mut std::io::stdout()).map_err(Application)?;
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
        facts.field("name", Cell::styled(info["name"].as_str().unwrap_or(""), Style::Cyan));
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
            Cell::number(
                info["size_bytes"]
                    .as_u64()
                    .map(dg::render::human_bytes)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        if info["tombstoned"].as_bool() == Some(true) {
            facts.field("purge after", Cell::number(purge_after_text(&info)));
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
        dg::shell::render(&result, false, &mut std::io::stdout()).map_err(Application)?;
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
