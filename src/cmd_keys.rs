//! Credential administration commands (owner only at the API).

use super::{Request, human_result, table_json};
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
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    role: String,
    database: Vec<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    for db in &database {
        dg::store::validate_name(db).map_err(Application)?;
    }
    // The view owns its copy of the names: it outlives this call, and
    // the request body needs them too.
    let view_name = name.clone();
    let view_role = role.clone();
    let view_database = database.clone();
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::POST,
            path: "/v1/keys".to_string(),
            body: Some(json!({"name":name,"role":role,"databases":database})),
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |result| {
            // The key is shown ONCE and never again, so it is on the
            // screen. The scope is shown with it, because a key
            // without its scope is a key nobody can use safely.
            let scope = view_database.join(", ");
            dg::render::Outcome::with_facts(
                format!("issued {view_name}"),
                vec![
                    (
                        "key".to_string(),
                        result["key"].as_str().unwrap_or("").to_string(),
                    ),
                    ("role".to_string(), view_role.clone()),
                    ("scope".to_string(), scope),
                ],
            )
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        "/v1/keys".into(),
        Some(json!({"name":name,"role":role,"databases":database})),
        None,
        dg::client::Semantics::Write,
    )))
}

pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if let Some(keys) = table_json(server, json, key_path, Method::GET, "/v1/keys").await? {
        let table = dg::views::keys(&keys);
        let mut out = std::io::stdout();
        table
            .write(&mut out, &terminal_theme())
            .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((
        Method::GET,
        "/v1/keys".into(),
        None,
        None,
        dg::client::Semantics::Read,
    )))
}

/// The credentials that can act on one database.
///
/// The owner key can do everything, so it reaches every database. An
/// application key reaches the databases it was scoped to. The answer
/// to "who can touch this database" is the question a leaked-key
/// incident starts with.
///
/// The answer is narrowed for BOTH forms: a script that asks which
/// credentials can reach a database gets the credentials that can, not
/// every credential the gateway holds.
pub async fn tokens(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    database: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&database).map_err(Application)?;
    let keys = super::fetch(server, key_path, Method::GET, "/v1/keys").await?;
    if super::api_failed(&keys) {
        super::show_error(&keys, json);
        return Err(Application(super::api_error()));
    }
    // The credentials that reach this database: the owner key reaches
    // everything, an application key reaches only its own.
    let reaching: Vec<Value> = keys["keys"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|key| {
            let role = key["role"].as_str().unwrap_or("");
            role == "owner"
                || key["databases"]
                    .as_array()
                    .map(|dbs| dbs.iter().any(|name| name.as_str() == Some(&database)))
                    .unwrap_or(false)
        })
        .collect();
    let answer = json!({"keys": reaching});
    if json || !std::io::stdout().is_terminal() {
        println!("{answer}");
        return Ok(None);
    }
    if reaching.is_empty() {
        // No table: the answer is "nobody", and an empty grid would
        // read as a rendering failure rather than an answer.
        let mut out = std::io::stdout();
        writeln!(out, "no credential can act on {database}")
            .map_err(|_| Application(dg::error::Error::internal()))?;
        return Ok(None);
    }
    let mut table = Table::new();
    table.column("name", Align::Left);
    table.column("role", Align::Left);
    table.column("scope", Align::Left);
    for key in reaching.iter() {
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
    let mut out = std::io::stdout();
    table
        .write(&mut out, &terminal_theme())
        .map_err(Application)?;
    Ok(None)
}

pub async fn revoke(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let view_name = name.clone();
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::DELETE,
            path: format!("/v1/keys/{name}"),
            body: None,
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |_result| dg::render::Outcome::done(format!("revoked {view_name}"))),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::DELETE,
        format!("/v1/keys/{name}"),
        None,
        None,
        dg::client::Semantics::Write,
    )))
}

/// Revoke every credential scoped to one database.
///
/// The bulk form of `revoke`, for a leaked key: the operator knows which
/// database is exposed, not which credential names exist. One request,
/// revoked server-side in one transaction, so a half-finished sweep
/// cannot leave some of the leaked keys alive.
pub async fn revoke_database(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    database: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&database).map_err(Application)?;
    let view_database = database.clone();
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::DELETE,
            path: format!("/v1/database-keys/{database}"),
            body: None,
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |result| {
            // The names are reported, not counted: after a leaked key is
            // cut, the operator wants to know WHICH credentials went, so
            // they can check nothing else depended on them.
            let names: Vec<String> = result["revoked"]
                .as_array()
                .map(|keys| {
                    keys.iter()
                        .filter_map(|key| key.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if names.is_empty() {
                return dg::render::Outcome::done(format!(
                    "no credentials scoped to {view_database}; nothing revoked"
                ));
            }
            dg::render::Outcome::with_facts(
                format!(
                    "revoked {} credential{} scoped to {view_database}",
                    names.len(),
                    if names.len() == 1 { "" } else { "s" }
                ),
                vec![("revoked".to_string(), names.join(", "))],
            )
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::DELETE,
        format!("/v1/database-keys/{database}"),
        None,
        None,
        dg::client::Semantics::Write,
    )))
}

pub async fn rotate_owner(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::POST,
            path: "/v1/keys/owner/rotate".to_string(),
            body: None,
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(|result: &Value| {
            // The old owner key dies the moment this one is issued, so
            // the new key is on the screen, and the operator has to be
            // told the old one is already gone.
            dg::render::Outcome::with_facts(
                "rotated the owner credential",
                vec![
                    (
                        "key".to_string(),
                        result["key"].as_str().unwrap_or("").to_string(),
                    ),
                    ("old key".to_string(), "dead immediately".to_string()),
                ],
            )
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        "/v1/keys/owner/rotate".into(),
        None,
        None,
        dg::client::Semantics::Write,
    )))
}
