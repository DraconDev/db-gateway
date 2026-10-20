//! Credential administration commands (owner only at the API).

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::render::{Align, Cell, Style, Table, Theme};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
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

/// The credentials that can act on one database.
///
/// The owner key can do everything, so it reaches every database. An
/// application key reaches the databases it was scoped to. The answer
/// to "who can touch this database" is the question a leaked-key
/// incident starts with.
pub async fn tokens(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    database: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&database).map_err(Application)?;
    if let Some(keys) = table_json(server, json, key_path, Method::GET, "/v1/keys").await? {
        let mut table = Table::new();
        table.column("name", Align::Left);
        table.column("role", Align::Left);
        table.column("scope", Align::Left);
        let mut found = 0usize;
        for key in keys["keys"].as_array().cloned().unwrap_or_default().iter() {
            let role = key["role"].as_str().unwrap_or("");
            let databases: Vec<String> = key["databases"]
                .as_array()
                .map(|dbs| {
                    dbs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            // The owner key is scoped to everything, so it reaches every
            // database; an application key reaches only its own.
            let reaches = role == "owner" || databases.iter().any(|name| name == &database);
            if !reaches {
                continue;
            }
            found += 1;
            let scope = if role == "owner" {
                "all".to_string()
            } else if databases.is_empty() {
                "-".to_string()
            } else {
                databases.join(", ")
            };
            table.row(vec![
                Cell::styled(key["name"].as_str().unwrap_or(""), Style::Cyan),
                Cell::styled(
                    role,
                    if role == "owner" {
                        Style::Yellow
                    } else {
                        Style::Plain
                    },
                ),
                Cell::text(scope),
            ]);
        }
        if found == 0 {
            // No table: the answer is "nobody", and an empty grid would
            // read as a rendering failure rather than an answer.
            let mut out = std::io::stdout();
            writeln!(out, "no credential can act on {database}")
                .map_err(|_| Application(dg::error::Error::internal()))?;
            return Ok(None);
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
