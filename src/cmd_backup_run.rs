//! `dg backup run`: take a backup set now.
//!
//! The hourly producer on the host writes a backup set every hour; this
//! command asks the gateway to write one immediately, so an operator
//! does not have to wait for the next run to have a copy of what is live
//! right now. The set is the producer's set: same layout, same manifest,
//! same checksums, and the gateway's own verifier accepts it.
//!
//! The CLI's rule is the rule: a terminal gets the human view, a pipe
//! or `--json` gets the API's JSON unchanged.

use dg::error::Error;
use dg::render::{Align, Cell, Outcome, Table, Theme};
use reqwest::Method;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// `dg backup run`: publish one backup set, now.
///
/// The gateway does the work: it takes its own lock, copies every
/// database consistently, validates the set and publishes it with an
/// atomic rename. This command is the request and the report.
pub async fn run(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<(), dg::ServerError> {
    let path = key_path
        .clone()
        .or_else(dg::client::default_key_file)
        .ok_or_else(|| {
            dg::ServerError::Application(Error::new(
                400,
                "missing_credential",
                "--key-file required",
            ))
        })?;
    let key = dg::client::read_key(&path).map_err(dg::ServerError::Application)?;
    // The run publishes a set: a durable write whose outcome matters if
    // the connection drops, so it is asked for with write semantics.
    let result = dg::client::request_with_semantics(
        server,
        &key,
        Method::POST,
        "/v1/backups/run",
        None,
        None,
        dg::client::Semantics::Write,
    )
    .await
    .map_err(dg::ServerError::Application)?;
    finish(&result, json)
}

/// Show the answer the way the operator asked for it.
///
/// A machine gets the API's JSON unchanged and a nonzero exit when the
/// answer is an error; a person gets the view, or the error the shell
/// draws for every other command.
fn finish(result: &Value, json: bool) -> Result<(), dg::ServerError> {
    let failed = result.get("error").is_some();
    if json || !std::io::stdout().is_terminal() {
        println!("{result}");
        return if failed { Err(api_error()) } else { Ok(()) };
    }
    if failed {
        dg::shell::render_with(result, false, &terminal_theme(), &mut std::io::stdout())
            .map_err(dg::ServerError::Application)?;
        return Err(api_error());
    }
    let theme = terminal_theme();
    let mut out = std::io::stdout();
    view(result)
        .write(&mut out, &theme)
        .map_err(dg::ServerError::Application)?;
    let table = table_of(result);
    if !table.rows().is_empty() {
        writeln!(out).map_err(|_| dg::ServerError::Application(Error::internal()))?;
        table
            .write(&mut out, &theme)
            .map_err(dg::ServerError::Application)?;
    }
    Ok(())
}

/// The human view of a published set.
///
/// The set's name, what it carries, how long it took and how many bytes
/// it holds. A run that captured nothing says so plainly rather than
/// showing an empty table, and a run that reclaimed nothing says that
/// too: retention belongs to the hourly producer, so this command never
/// removes an older set.
pub fn view(result: &Value) -> Outcome {
    let set = result["set"].as_str().unwrap_or_default();
    let databases = result["databases"].as_array().cloned().unwrap_or_default();
    let bytes = result["bytes"].as_u64().unwrap_or(0);
    let duration_ms = result["duration_ms"].as_u64().unwrap_or(0);
    let registry = result["registry"]["file"].as_str().unwrap_or_default();
    let registry_size = result["registry"]["size"].as_u64().unwrap_or(0);
    let captured = if databases.is_empty() {
        "nothing (no databases were captured)".to_string()
    } else {
        format!(
            "{} database{}",
            databases.len(),
            if databases.len() == 1 { "" } else { "s" }
        )
    };
    Outcome::with_facts(
        format!("published backup set {set}"),
        vec![
            ("set".to_string(), set.to_string()),
            (
                "created".to_string(),
                result["created"].as_str().unwrap_or_default().to_string(),
            ),
            ("captured".to_string(), captured),
            ("bytes".to_string(), dg::render::human_bytes(bytes)),
            ("took".to_string(), took(duration_ms)),
            (
                "registry".to_string(),
                format!("{registry} ({})", dg::render::human_bytes(registry_size)),
            ),
            (
                "pruned".to_string(),
                "nothing (the hourly producer owns retention)".to_string(),
            ),
        ],
    )
}

/// The set's members, as a table.
pub fn table_of(result: &Value) -> Table {
    let mut table = Table::new();
    table.column("database", Align::Left);
    table.column("size", Align::Right);
    table.column("sha256", Align::Left);
    for database in result["databases"].as_array().into_iter().flatten() {
        table.row(vec![
            Cell::text(database["name"].as_str().unwrap_or_default()),
            Cell::text(dg::render::human_bytes(
                database["size"].as_u64().unwrap_or(0),
            )),
            Cell::text(database["sha256"].as_str().unwrap_or_default()),
        ]);
    }
    table
}

/// How long the run took, in the units an operator reads.
fn took(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms} ms")
    } else {
        dg::render::human_duration(duration_ms / 1_000)
    }
}

/// The error a failed API answer carries.
fn api_error() -> dg::ServerError {
    dg::ServerError::Application(Error::new(
        400,
        "api_error",
        "API request failed; see JSON response",
    ))
}
