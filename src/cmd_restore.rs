//! The restore commands: what the off-host replica can be taken back to.
//!
//! The restore itself is the host's job -- it holds the bucket's
//! credentials and the sidecar -- and the host publishes the window and
//! the result. This command reads those facts through the API, so an
//! operator on the other end of a tunnel can ask the same question the
//! dashboard answers: can I get my data back from last Tuesday?

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
