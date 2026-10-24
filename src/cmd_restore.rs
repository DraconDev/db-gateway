//! The restore commands: what the off-host replica can be taken back to.
//!
//! The restore itself is the host's job -- it holds the bucket's
//! credentials and the sidecar -- and the host publishes the window and
//! the result. This command reads those facts through the API, so an
//! operator on the other end of a tunnel can ask the same question the
//! dashboard answers: can I get my data back from last Tuesday?

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::error::Error;
use dg::render::{Cell, Section, Style, Table, Theme};
use reqwest::Method;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// The style for a run's outcome.
///
/// The word is always spelled out, so the colour is a second rendering of
/// the same fact rather than the only rendering of it.
fn outcome_style(outcome: &str) -> Style {
    match outcome {
        "ok" => Style::Green,
        "failed" | "error" => Style::Red,
        _ => Style::Yellow,
    }
}

/// The restore points the off-host replica offers, and the last restore
/// that was taken from it.
pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if let Some(backups) = table_json(server, json, key_path, Method::GET, "/v1/backups").await? {
        let sections = dg::views::restore(&backups);
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        "/v1/backups".into(),
        None,
        None,
        dg::client::Semantics::Read,
    )))
}

/// The restore points as a table, for a caller that wants the rows rather
/// than the prose. Kept next to `list` so the two cannot drift.
pub fn table(backups: &serde_json::Value) -> Table {
    let mut table = Table::new(vec!["database", "window", "files"]);
    if let Some(databases) = backups["restore_points"]["databases"].as_object() {
        for (name, window) in databases {
            let span = match (window["oldest"].as_str(), window["newest"].as_str()) {
                (Some(oldest), Some(newest)) => format!(
                    "{} to {}",
                    dg::render::human_instant(oldest),
                    dg::render::human_instant(newest)
                ),
                _ => "unknown".to_string(),
            };
            table.row(vec![
                Cell::text(name.clone()),
                Cell::text(span),
                Cell::text(window["files"].as_u64().unwrap_or(0).to_string()),
            ]);
        }
    }
    table
}

/// The last restore, as a section, for a caller that wants the verdict.
pub fn last_restore(backups: &serde_json::Value) -> Section {
    let mut section = Section::new("last restore");
    match backups["last_restore"].as_object() {
        Some(restore) => {
            let outcome = restore["outcome"].as_str().unwrap_or("unknown");
            section.field(
                "outcome",
                Cell::styled(outcome.to_string(), outcome_style(outcome)),
            );
            section.field(
                "database",
                Cell::text(restore["database"].as_str().unwrap_or("unknown")),
            );
        }
        None => {
            section.field("outcome", Cell::text("none taken yet"));
        }
    }
    section
}

/// The error a caller gets when the gateway cannot be reached. Kept here
/// so the command and its callers agree on the wording.
pub fn unavailable() -> Error {
    Error::internal()
}
