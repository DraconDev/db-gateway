//! Credential administration commands (owner only at the API).

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::render::{Align, Cell, Style, Table, Theme};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

pub async fn create(
    name: String,
    role: String,
    database: Vec<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    for db in &database {
        dg::store::validate_name(db).map_err(Application)?;
    }
    Ok(Some((
        Method::POST,
        "/v1/keys".into(),
        Some(json!({"name":name,"role":role,"databases":database})),
        None,
    )))
}

pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if let Some(keys) = table_json(server, json, key_path, Method::GET, "/v1/keys").await? {
        let mut table = Table::new();
        table.column("name", Align::Left);
        table.column("role", Align::Left);
        table.column("databases", Align::Left);
        for key in keys["keys"].as_array().cloned().unwrap_or_default().iter() {
            let databases = key["databases"]
                .as_array()
                .map(|dbs| {
                    dbs.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let role = key["role"].as_str().unwrap_or("");
            table.row(vec![
                Cell::styled(key["name"].as_str().unwrap_or(""), Style::Cyan),
                // The owner key is the one that can do everything, so it
                // is worth seeing at a glance in a list of credentials.
                Cell::styled(
                    role,
                    if role == "owner" {
                        Style::Yellow
                    } else {
                        Style::Plain
                    },
                ),
                Cell::text(if databases.is_empty() && role == "owner" {
                    "all".to_string()
                } else {
                    databases
                }),
            ]);
        }
        let mut out = std::io::stdout();
        table
            .write(&mut out, &terminal_theme())
            .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/keys".into(), None, None)))
}

pub async fn revoke(name: String) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    Ok(Some((
        Method::DELETE,
        format!("/v1/keys/{name}"),
        None,
        None,
    )))
}

pub async fn rotate_owner() -> Result<Option<Request>, dg::ServerError> {
    Ok(Some((
        Method::POST,
        "/v1/keys/owner/rotate".into(),
        None,
        None,
    )))
}
