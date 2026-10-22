//! The `dg status` service summary, and the `dg watch` snapshot of it.

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::render::Theme;
use reqwest::Method;
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
        dg::render::write_sections(
            &mut out,
            &terminal_theme(),
            &dg::views::service_dashboard(&status),
        )
        .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        "/v1/status".into(),
        None,
        None,
        dg::client::Semantics::Read,
    )))
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
