//! The `dg status` service summary, and the `dg watch` snapshot of it.

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::render::{
    Align, Cell, Section, Style, Table, Theme, human_count, human_duration, human_micros,
    human_percent, human_rate,
};
use reqwest::Method;
use serde_json::Value;
use std::io::IsTerminal;
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

pub async fn status(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if let Some(status) = table_json(server, json, key_path, Method::GET, "/v1/status").await? {
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &dashboard(&status))
            .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/status".into(), None, None)))
}

/// The `dg watch` snapshot: the same dashboard as `dg status`, read once.
///
/// It exits rather than refreshing in place, so it stays pipeable and
/// diffable: `dg watch > snapshot.json` is a snapshot, and a second run
/// is a diff.
pub async fn watch(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    status(server, json, key_path).await
}

/// How long ago a timestamp was, as a person reads it.
///
/// The absolute time is kept alongside, because "3m ago" alone does not
/// say when a write happened, only how recently.
fn human_epoch_ago(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch);
    let age = now.saturating_sub(epoch);
    format!("{} ago", human_duration(age))
}
