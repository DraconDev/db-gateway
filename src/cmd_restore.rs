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
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// The plan for a restore to a moment: what each database's window
/// reaches.
///
/// The window is per database, so the plan is per database too: a
/// database whose replica starts later than the others cannot be taken
/// back to a moment they can. This says which is which before anyone
/// asks for the restore.
pub async fn plan(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    to: Option<&str>,
) -> Result<Option<Request>, dg::ServerError> {
    // The machine form is the PLAN, not the window it was read from: the
    // question the operator asked is "does this database cover that
    // moment", and an answer a script has to recompute is not an answer.
    if json || !std::io::stdout().is_terminal() {
        let path = key_path.as_deref().ok_or_else(|| {
            Application(dg::error::Error::new(
                400,
                "missing_credential",
                "--key-file required",
            ))
        })?;
        let key = dg::client::read_key(path).map_err(Application)?;
        let backups = dg::client::request(server, &key, Method::GET, "/v1/backups", None, None)
            .await
            .map_err(Application)?;
        if backups.get("error").is_some() {
            return Err(Application(dg::error::Error::new(
                400,
                "api_error",
                "the backup facts could not be read",
            )));
        }
        let plan = dg::views::RestorePlan::read(&backups, to);
        let mut out = std::io::stdout();
        writeln!(out, "{}", plan.to_json())
            .map_err(|_| Application(dg::error::Error::internal()))?;
        return Ok(None);
    }
    if let Some(backups) = table_json(server, json, key_path, Method::GET, "/v1/backups").await? {
        let sections = dg::views::restore_plan(&backups, to);
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
