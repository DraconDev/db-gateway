//! Everyday data commands against the API: list/info/create,
//! query/batch, and the interactive shell.

use super::{Request, api_error, api_failed, human_result};
use dg::ServerError::Application;
use dg::error::Error;
use dg::render::{Align, Cell, Section, Style, Table, Theme};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// Refuse a batch the API cannot take, before it is sent.
///
/// The rule lives in the library, next to the cap it enforces, so the
/// CLI and the terminal view refuse an over-long script the same way
/// and from the same number.
fn check_statement_count(count: usize, subject: &str) -> Result<(), Error> {
    dg::execution::check_statement_count(count, subject)
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
        let table = dg::views::database_names(&result);
        let mut out = std::io::stdout();
        table
            .write(&mut out, &terminal_theme())
            .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        "/v1/databases".into(),
        None,
        None,
        dg::client::Semantics::Read,
    )))
}

/// How much data each database holds: its tables and its rows.
///
/// The traffic counters say what a database has been doing, and they
/// are in memory: a restart resets them, so a database full of data
/// reads as nothing beside them. This reads the files and says what
/// the databases hold.
pub async fn contents(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let result = dg::client::request(server, &key, Method::GET, "/v1/contents", None, None)
            .await
            .map_err(Application)?;
        if api_failed(&result) {
            dg::shell::render_with(&result, false, &terminal_theme(), &mut std::io::stdout())
                .map_err(Application)?;
            return Err(Application(api_error()));
        }
        let table = dg::views::contents(&result);
        let mut out = std::io::stdout();
        table
            .write(&mut out, &terminal_theme())
            .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        "/v1/contents".into(),
        None,
        None,
        dg::client::Semantics::Read,
    )))
}

/// The per-database live view: what this database holds and what it
/// has been doing.
///
/// `dg info` is what a database IS; `dg inspect` is what it has been
/// DOING. It reads the same facts the service dashboard shows, but for
/// one database, so an operator can look at the one that is busy
/// without reading the whole list.
///
/// The rich view is not terminal-only. A script that asks what a
/// database has been doing gets the same facts a person sees, in JSON:
/// read-only, so there is nothing to confirm, and expensive enough to
/// reason about that a silent downgrade to the bare info document
/// would be a trap. Both forms are built from one `inspected` value so
/// the two answers cannot drift apart.
pub async fn inspect(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let facts = inspected(server, json, key_path, &name).await?;

    if !json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &facts.sections())
            .map_err(Application)?;
        return Ok(None);
    }
    println!("{}", facts.document(&name));
    Ok(None)
}

/// Everything `dg inspect` knows about one database, fetched once.
///
/// The usage and backup facts are auxiliary: a failed fetch degrades
/// those sections instead of failing the command, in both the terminal
/// and the JSON form. `None` means the fetch failed or was refused, and
/// is reported as absent rather than as a zero.
struct Inspected {
    info: Value,
    status: Option<Value>,
    backups: Option<Value>,
}

/// Fetch the three reads `dg inspect` composes.
async fn inspected(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: &str,
) -> Result<Inspected, dg::ServerError> {
    let info = super::fetch(
        server,
        key_path,
        Method::GET,
        &format!("/v1/databases/{name}"),
    )
    .await?;
    if api_failed(&info) {
        super::show_error(&info, json);
        return Err(Application(api_error()));
    }
    let status = super::fetch(server, key_path, Method::GET, "/v1/status")
        .await
        .ok()
        .filter(|status| !api_failed(status));
    let backups = super::fetch(server, key_path, Method::GET, "/v1/backups")
        .await
        .ok()
        .filter(|backups| !api_failed(backups));
    Ok(Inspected {
        info,
        status,
        backups,
    })
}

impl Inspected {
    /// The one database's usage entry, or an empty object when the
    /// status fetch failed: absent activity reads as zero, which is
    /// true for a database nothing has touched.
    fn activity_entry(&self, name: &str) -> Value {
        Value::Object(
            self.status
                .as_ref()
                .and_then(|status| status["databases"][name].as_object().cloned())
                .unwrap_or_default(),
        )
    }

    /// The one database's backup health, or an empty object.
    fn backup_health(&self, name: &str) -> Value {
        Value::Object(
            self.backups
                .as_ref()
                .and_then(|backups| backups["per_database"][name].as_object().cloned())
                .unwrap_or_default(),
        )
    }

    fn tombstoned(&self) -> bool {
        self.info["tombstoned"].as_bool() == Some(true)
    }

    /// The same facts as a JSON document.
    ///
    /// `backup` and `activity` are always present so a script can read
    /// a path without checking first. Their `*_available` flags carry
    /// the one distinction that matters: a section whose auxiliary
    /// fetch FAILED says unavailable, while a database the gateway has
    /// simply never touched reports the zeros and `null`s that are
    /// true of it. Building the activity object here from the same
    /// reading the terminal section uses is what keeps the two from
    /// telling different stories about one database.
    fn document(&self, name: &str) -> Value {
        let health = self.backup_health(name);
        let mut tables = Vec::new();
        for entry in self.info["tables"].as_array().into_iter().flatten() {
            tables.push(json!({
                "name": entry["name"],
                "rows": entry["rows"],
            }));
        }
        json!({
            "name": name,
            "created_at": self.info["created_at"],
            "size_bytes": self.info["size_bytes"],
            "tombstoned": self.tombstoned(),
            "protected": self.info["protected"].as_bool().unwrap_or(false),
            "purge_after": self.info["purge_after"],
            "tables": tables,
            "activity": dg::views::activity_document(&self.activity_entry(name)),
            "activity_available": self.status.is_some(),
            "backup": json!({
                "state": health.get("state").and_then(Value::as_str).unwrap_or("unknown"),
                "newest_set": health.get("newest_set"),
                "age_secs": health.get("age_secs"),
            }),
            "backup_available": self.backups.is_some(),
        })
    }

    /// The same facts as terminal sections.
    fn sections(&self) -> Vec<Section> {
        let name = self.info["name"].as_str().unwrap_or("");
        let mut facts = Section::new("database");
        facts.field("name", Cell::styled(name, Style::Cyan));
        facts.field(
            "size",
            Cell::text(
                self.info["size_bytes"]
                    .as_u64()
                    .map(dg::render::human_bytes)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        facts.field(
            "state",
            if self.tombstoned() {
                Cell::styled("tombstoned", Style::Yellow)
            } else {
                Cell::styled("live", Style::Green)
            },
        );
        facts.field(
            "delete protection",
            Cell::text(if self.info["protected"].as_bool() == Some(true) {
                "protected"
            } else {
                "off"
            }),
        );
        if self.tombstoned() {
            facts.field("purge after", Cell::text(purge_after_text(&self.info)));
        } else {
            facts.field("created", Cell::text(created_text(&self.info)));
        }
        let mut sections = vec![facts];

        // What this database has been doing.
        sections.push(dg::views::activity_section(&self.activity_entry(name)));

        // How its backup is doing.
        let health = self.backup_health(name);
        let mut backup = Section::new("backup");
        let state = health
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        backup.field(
            "state",
            Cell::styled(state, dg::views::backup_state_style(state)),
        );
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

        // What it holds. A tombstone reports no tables: its file is
        // still on disk but the name is not queryable.
        if !self.tombstoned() {
            let mut tables_section = Section::new("tables");
            match self.info["tables"].as_array() {
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
        sections
    }
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
        dg::client::Semantics::Read,
    )))
}

/// The structure of one database's tables: every column with its type,
/// its flags and its default, plus the table's indexes and foreign
/// keys.
///
/// One table when a name is given, every table when it is not. The
/// answer comes from the shared view layer, so the terminal drawing
/// and the terminal view read the same document the same way.
pub async fn schema(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    table: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    // The table name narrows the answer on the server, so the document
    // a pipe reads is the API's own document either way.
    let path = match &table {
        Some(wanted) if wanted.is_empty() => {
            return Err(Application(Error::new(
                400,
                "invalid_arguments",
                "a table name is letters, digits and underscores",
            )));
        }
        Some(wanted) => {
            if !wanted
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
                || wanted.starts_with(|character: char| character.is_ascii_digit())
            {
                return Err(Application(Error::new(
                    400,
                    "invalid_arguments",
                    "a table name is letters, digits and underscores, and does not start with a digit",
                )));
            }
            format!("/v1/databases/{name}/schema?table={wanted}")
        }
        None => format!("/v1/databases/{name}/schema"),
    };
    if json || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        // The JSON is the API's document, unchanged: a pipe reads the
        // same thing a terminal does, minus the drawing.
        return Ok(Some((
            Method::GET,
            path,
            None,
            None,
            dg::client::Semantics::Read,
        )));
    }
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    let document = dg::client::request(server, &key, Method::GET, &path, None, None)
        .await
        .map_err(Application)?;
    if api_failed(&document) {
        dg::shell::render_with(&document, false, &terminal_theme(), &mut std::io::stdout())
            .map_err(Application)?;
        return Err(Application(api_error()));
    }
    let sections = dg::views::structure_sections(&document, table.as_deref());
    let mut out = std::io::stdout();
    dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
    Ok(None)
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

pub async fn create(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::POST,
            path: "/v1/databases".to_string(),
            body: Some(json!({"name":name})),
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(|result| {
            dg::render::Outcome::done(format!("created {}", result["name"].as_str().unwrap_or("")))
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        "/v1/databases".into(),
        Some(json!({"name":name})),
        None,
        dg::client::Semantics::Write,
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
    let script = std::fs::read_to_string(&file).map_err(|_| {
        Application(Error::new(
            400,
            "unreadable_file",
            "cannot read the SQL file",
        ))
    })?;
    let statements = dg::sqlsplit::split_statements(&script);
    if statements.is_empty() {
        return Err(Application(Error::new(
            400,
            "empty_script",
            "the SQL file holds no statements",
        )));
    }
    // The count is checked before the request is built, so an over-long
    // script is refused here — with the count in the message — instead
    // of being sent and coming back as the API's bare rejection.
    check_statement_count(statements.len(), "the file").map_err(Application)?;
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
        let semantics = dg::client::semantics_of(&statements);
        let result = dg::client::request_with_semantics(
            server,
            &key,
            Method::POST,
            &format!("/v1/databases/{name}/batch"),
            Some(body),
            idempotency_key.as_deref(),
            semantics,
        )
        .await
        .map_err(Application)?;
        // The per-statement outcomes are the point of running a script,
        // so they are drawn rather than dumped.
        let mut out = std::io::stdout();
        let theme = terminal_theme();
        // The answer is read once, in the shared view layer, so this
        // table and the terminal UI's table are the same table: a
        // statement the gateway refused cannot be drawn as "rolled
        // back" on one surface and "not run" on another.
        let outcome = dg::views::batch_outcomes(&statements, &result);
        outcome.table.write(&mut out, &theme).map_err(Application)?;
        if let Some(code) = &outcome.code {
            // When the rejection names no statement, every row says
            // only "not run" — so the reason the request was refused
            // belongs in the error the command returns, or the
            // operator is left with a table that says nothing ran and
            // no idea why.
            let failure = if outcome.refused {
                Error::with_message(
                    400,
                    "api_error",
                    format!(
                        "API request failed: {code}: {}",
                        outcome.message.as_deref().unwrap_or("failed")
                    ),
                )
            } else {
                Error::new(400, "api_error", "API request failed; see response above")
            };
            return Err(Application(failure));
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
        dg::client::semantics_of(&statements),
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
        // The statements decide whether this is a read or a write, not
        // the method: a query is a POST either way, and a read that is
        // given up on must not be reported as a write that may have
        // committed.
        let semantics = dg::client::semantics_of(std::slice::from_ref(&sql));
        let result = dg::client::request_with_semantics(
            server,
            &key,
            Method::POST,
            &format!("/v1/databases/{name}/query"),
            Some(json!({"sql":sql,"args":args})),
            idempotency_key.as_deref(),
            semantics,
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
        dg::client::semantics_of(std::slice::from_ref(&sql)),
    )))
}

pub async fn batch(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    file: PathBuf,
    idempotency_key: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    // The file's rules are the library's: a regular file, at most
    // 64 KiB, holding the API's own batch shape.
    let body = dg::execution::read_batch_file(&file).map_err(Application)?;
    // The same cap the SQL-file path enforces, checked before the
    // request is built: the batch API takes at most
    // `MAX_BATCH_STATEMENTS` statements, and a longer batch file is
    // refused here rather than at the server.
    check_statement_count(body.statements.len(), "the batch file").map_err(Application)?;
    let view_body = body.clone();
    let body_value = serde_json::to_value(&body).map_err(|_| Application(Error::invalid()))?;
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::POST,
            path: format!("/v1/databases/{name}/batch"),
            body: Some(body_value.clone()),
            idempotency: idempotency_key.clone(),
            semantics: dg::client::semantics_of_statements(&body.statements),
        },
        Box::new(move |result| {
            // A batch is a list of statements, so its result is a list
            // of results: one row per statement, in order. The reading
            // is the shared one, so this table and the terminal UI's
            // table cannot disagree about what a statement did.
            let statements: Vec<String> = view_body
                .statements
                .iter()
                .map(|statement| statement.sql.clone())
                .collect();
            let outcome = dg::views::batch_outcomes(&statements, result);
            dg::render::Outcome::list("statements", outcome.table)
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/batch"),
        Some(body_value),
        idempotency_key,
        dg::client::semantics_of_statements(&body.statements),
    )))
}

/// What the rows browser was asked for.
///
/// One value rather than seven arguments: the browser's question is a
/// single thing -- this table, this page, in this order, narrowed this
/// way -- and passing it as one value keeps the call sites honest about
/// what belongs together.
pub struct RowsRequest {
    /// The database the table is in.
    pub name: String,
    /// The table to read.
    pub table: String,
    /// Which page, counting from zero.
    pub page: u64,
    /// The column to sort by, and which way.
    pub sort: Option<String>,
    /// The condition narrowing the rows.
    pub condition: Option<String>,
}

/// Read one table's rows, a page at a time.
///
/// The rows browser on the command line. It reads the table's structure
/// first, because the sort column has to be checked against the table's
/// real columns before it can be written into the statement -- and then
/// the count and the page, so the footer's `of N` is the size of the
/// filtered answer rather than the table's total.
///
/// A pipe gets the gateway's own query document unchanged, so a script
/// reads exactly what the API said. A terminal gets the grid.
pub async fn rows(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    request: RowsRequest,
) -> Result<Option<Request>, dg::ServerError> {
    let RowsRequest {
        name,
        table,
        page,
        sort,
        condition,
    } = request;
    dg::store::validate_name(&name).map_err(Application)?;
    // The table is validated against the database's own schema below, so
    // a name that is not a table never reaches a statement. This check
    // only keeps the obvious garbage out of the URL.
    if table.is_empty()
        || !table
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        || table.starts_with(|character: char| character.is_ascii_digit())
    {
        return Err(Application(Error::new(
            400,
            "invalid_arguments",
            "a table name is letters, digits and underscores, and does not start with a digit",
        )));
    }
    let sort = match sort {
        None => None,
        Some(wanted) => {
            let (column, direction) = match wanted.split_once(':') {
                None => (wanted.as_str(), dg::views::SortDirection::Ascending),
                Some((column, "asc")) => (column, dg::views::SortDirection::Ascending),
                Some((column, "desc")) => (column, dg::views::SortDirection::Descending),
                Some((_, other)) => {
                    // The direction the operator typed is echoed back so
                    // the message says what to change.
                    let message = format!("a sort direction is asc or desc, not {other}");
                    return Err(Application(Error::with_message(
                        400,
                        "invalid_arguments",
                        message,
                    )));
                }
            };
            Some((column.to_string(), direction))
        }
    };
    let machine = json || !std::io::IsTerminal::is_terminal(&std::io::stdout());
    if machine {
        // A pipe reads the page as the gateway's own document. The sort
        // and the filter still have to be validated, which means reading
        // the structure -- so this is not a single request either.
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
            400,
            "missing_credential",
            "--key-file required",
        )))?)
        .map_err(Application)?;
        let structure_path = format!("/v1/databases/{name}/schema?table={table}");
        let structure = dg::client::request(server, &key, Method::GET, &structure_path, None, None)
            .await
            .map_err(Application)?;
        if api_failed(&structure) {
            dg::shell::render_with(&structure, false, &terminal_theme(), &mut std::io::stdout())
                .map_err(Application)?;
            return Err(Application(api_error()));
        }
        let query = dg::views::rows_query(
            &structure,
            &table,
            page,
            sort.as_ref()
                .map(|(column, direction)| (column.as_str(), *direction)),
            condition.as_deref(),
        )
        .map_err(|refusal| {
            Application(Error::with_message(
                400,
                "invalid_arguments",
                refusal.message(),
            ))
        })?;
        return Ok(Some((
            Method::POST,
            format!("/v1/databases/{name}/query"),
            Some(serde_json::json!({"sql": query.sql, "args": []})),
            None,
            dg::client::Semantics::Read,
        )));
    }
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    let structure_path = format!("/v1/databases/{name}/schema?table={table}");
    let structure = dg::client::request(server, &key, Method::GET, &structure_path, None, None)
        .await
        .map_err(Application)?;
    if api_failed(&structure) {
        dg::shell::render_with(&structure, false, &terminal_theme(), &mut std::io::stdout())
            .map_err(Application)?;
        return Err(Application(api_error()));
    }
    // The count under the same filter, so the footer is the answer's own
    // size. A count that fails is not fatal: the page is still worth
    // printing, and the footer then says the total is unknown.
    let total = match dg::views::rows_count_query(&structure, &table, condition.as_deref()) {
        Ok(count) => read_count(server, &key, &name, &count.sql).await.ok(),
        Err(refusal) => {
            return Err(Application(Error::with_message(
                400,
                "invalid_arguments",
                refusal.message(),
            )));
        }
    };
    let query = dg::views::rows_query(
        &structure,
        &table,
        page,
        sort.as_ref()
            .map(|(column, direction)| (column.as_str(), *direction)),
        condition.as_deref(),
    )
    .map_err(|refusal| {
        Application(Error::with_message(
            400,
            "invalid_arguments",
            refusal.message(),
        ))
    })?;
    // A row read is a read, and the transport has to be told so. The
    // method cannot say it -- a query is a POST -- so the SQL decides,
    // with the same classification the server uses. Without this the
    // read waited the write timeout and, when it gave up, reported
    // `outcome_unknown`: "writes may have committed, replay only with the
    // same idempotency key", for a SELECT that committed nothing and
    // for which no key is ever sent. Advice an operator cannot follow is
    // its own kind of wrong answer.
    let response = dg::client::request_with_semantics(
        server,
        &key,
        Method::POST,
        &format!("/v1/databases/{name}/query"),
        Some(serde_json::json!({"sql": query.sql, "args": []})),
        None,
        dg::client::semantics_of(&[query.sql.as_str()]),
    )
    .await
    .map_err(Application)?;
    if api_failed(&response) {
        dg::shell::render_with(&response, false, &terminal_theme(), &mut std::io::stdout())
            .map_err(Application)?;
        return Err(Application(api_error()));
    }
    let browser = dg::tui::Browser {
        database: name.clone(),
        table: table.clone(),
        page,
        row_offset: 0,
        sort,
        filter: condition,
        total,
    };
    let outcome = dg::tui::browser_outcome(&browser, &response);
    let mut out = std::io::stdout();
    if let Some(table) = &outcome.table {
        table
            .write(&mut out, &terminal_theme())
            .map_err(Application)?;
    }
    Ok(None)
}

/// Run a count and read the number out of it.
async fn read_count(
    server: &str,
    key: &str,
    name: &str,
    sql: &str,
) -> Result<u64, dg::ServerError> {
    // The count behind the footer's total is a read too, and it has to
    // be told so for the same reason the row read above is.
    let response = dg::client::request_with_semantics(
        server,
        key,
        Method::POST,
        &format!("/v1/databases/{name}/query"),
        Some(serde_json::json!({"sql": sql, "args": []})),
        None,
        dg::client::semantics_of(&[sql]),
    )
    .await
    .map_err(Application)?;
    dg::tui::count_total(&response).ok_or_else(|| Application(Error::internal()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_at_the_cap_is_accepted_and_one_over_it_is_refused() {
        // The cap is the API's, and the CLI reads the same number: a
        // file of exactly 32 statements is a legal batch, and a file of
        // 33 is refused before it is sent.
        assert!(
            check_statement_count(dg::execution::MAX_BATCH_STATEMENTS, "the file").is_ok(),
            "a batch at the cap was refused"
        );
        let error = check_statement_count(dg::execution::MAX_BATCH_STATEMENTS + 1, "the file")
            .expect_err("a batch over the cap was accepted");
        assert_eq!(error.code, "too_many_statements");
        // The message is the operator's instruction: how many the file
        // holds, what the API accepts, and that nothing was sent.
        let message = error.message.to_string();
        assert!(
            message.contains("33 statements"),
            "the refusal does not say how many the file holds: {message}"
        );
        assert!(
            message.contains("accepts 32"),
            "the refusal does not say what the API accepts: {message}"
        );
        assert!(
            message.contains("nothing was sent"),
            "the refusal does not say that nothing was sent: {message}"
        );
    }

    #[test]
    fn the_client_checks_the_servers_own_cap() {
        // One number, two places: the server admits
        // `MAX_BATCH_STATEMENTS` statements and the CLI refuses a file
        // of more than that, both reading the same constant. A script
        // the server would take is never refused locally, and a script
        // the server would reject never leaves the machine.
        assert_eq!(dg::execution::MAX_BATCH_STATEMENTS, 32);
        assert!(
            check_statement_count(dg::execution::MAX_BATCH_STATEMENTS, "the file").is_ok(),
            "a batch the server would take was refused locally"
        );
        assert!(
            check_statement_count(dg::execution::MAX_BATCH_STATEMENTS + 1, "the file").is_err(),
            "a batch the server would reject was not refused locally"
        );
    }
}
