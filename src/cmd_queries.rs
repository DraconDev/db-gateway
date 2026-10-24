//! The query commands: which statement is the expensive one.
//!
//! Every statement passes through the gateway on its way to SQLite, so
//! the gateway is the only place that can say what each one cost. A
//! hosted provider cannot answer this exactly -- it is a privacy
//! liability and a tenant-isolation problem for them -- and here there
//! is no middleman.
//!
//! What is shown is the SHAPE of a statement: its SQL with every
//! literal replaced by a placeholder. The row names a statement without
//! quoting the data it carried.

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

/// The statement shapes the service has run, per database.
pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    database: Option<&str>,
) -> Result<Option<Request>, dg::ServerError> {
    let path = match database {
        Some(name) => format!("/v1/queries?database={name}"),
        None => "/v1/queries".to_string(),
    };
    if let Some(payload) = table_json(server, json, key_path, Method::GET, &path).await? {
        let sections = dg::views::queries(&payload);
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        path,
        None,
        None,
        dg::client::Semantics::Read,
    )))
}
