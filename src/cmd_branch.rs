//! The branch commands: fork a database, and see the forks that exist.
//!
//! WHY a branch command exists next to clone: a clone is a copy of the
//! database as it is now, and it says nothing about where it came from.
//! A branch is a copy that is ALSO a fork -- a record of the parent it
//! diverged from, and of the moment it diverged at. The copy is made
//! on the server, where the files are, so the branch is data and not a
//! description of data.
//!
//! The one rule every command here follows is the CLI's own rule: a
//! terminal gets the human view, a pipe or `--json` gets the JSON, and
//! the JSON is the API's answer unchanged.

use dg::error::Error;
use dg::render::{Align, Cell, Outcome, Table};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> dg::render::Theme {
    dg::render::Theme::for_stream(std::io::stdout().is_terminal())
}

/// `dg branch create <parent> <name> [--from <ISO8601>]`.
///
/// `--from` forks the newest backup set taken at or before that moment.
/// Without it the fork is the live parent.
pub async fn create(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    parent: String,
    name: String,
    from: Option<String>,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&parent).map_err(dg::ServerError::Application)?;
    dg::store::validate_name(&name).map_err(dg::ServerError::Application)?;
    // The moment is read here as well as on the server: a typo caught
    // before the request is a better answer than a typo caught after a
    // copy has been made.
    if let Some(at) = from.as_deref() {
        dg::branch::parse_moment(at).map_err(dg::ServerError::Application)?;
    }
    let body = json!({"parent": parent, "name": name, "at": from});
    let result = request(
        server,
        key_path,
        Method::POST,
        "/v1/branch",
        Some(body),
        dg::client::Semantics::Write,
    )
    .await?;
    let view_parent = parent.clone();
    let view_name = name.clone();
    finish(&result, json, move |result| {
        Outcome::with_facts(
            format!("forked {view_parent} to {view_name}"),
            vec![
                ("source".to_string(), source_of(result)),
                (
                    "forked at".to_string(),
                    dg::render::human_epoch(result["forked_at"].as_u64().unwrap_or(0) as i64),
                ),
                (
                    "size".to_string(),
                    dg::render::human_bytes(result["size_bytes"].as_u64().unwrap_or(0)),
                ),
            ],
        )
    })
}

/// `dg branch list`: every branch the gateway knows about.
pub async fn list(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<(), dg::ServerError> {
    let result = request(
        server,
        key_path,
        Method::GET,
        "/v1/branches",
        None,
        dg::client::Semantics::Read,
    )
    .await?;
    finish(&result, json, |result| {
        let mut table = Table::new();
        table.column("name", Align::Left);
        table.column("parent", Align::Left);
        table.column("forked at", Align::Left);
        table.column("source", Align::Left);
        for branch in result["branches"].as_array().into_iter().flatten() {
            table.row(vec![
                Cell::text(branch["name"].as_str().unwrap_or_default()),
                Cell::text(branch["parent"].as_str().unwrap_or_default()),
                Cell::text(dg::render::human_epoch(
                    branch["forked_at"].as_u64().unwrap_or(0) as i64,
                )),
                Cell::text(source_of(branch)),
            ]);
        }
        Outcome::list("branches", table)
    })
}

/// `dg branch show <name>`: one branch in detail.
pub async fn show(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(dg::ServerError::Application)?;
    let result = request(
        server,
        key_path,
        Method::GET,
        &format!("/v1/branches/{name}"),
        None,
        dg::client::Semantics::Read,
    )
    .await?;
    let view_name = name.clone();
    finish(&result, json, move |result| {
        Outcome::with_facts(
            format!("branch {view_name}"),
            vec![
                (
                    "parent".to_string(),
                    result["parent"].as_str().unwrap_or_default().to_string(),
                ),
                ("source".to_string(), source_of(result)),
                (
                    "forked at".to_string(),
                    dg::render::human_epoch(result["forked_at"].as_u64().unwrap_or(0) as i64),
                ),
            ],
        )
    })
}

/// `dg branch delete <name> --confirm <name>`.
///
/// The branch's DATABASE goes with it: the record and the copy are one
/// thing, and a record that outlived its database would describe a
/// fork that no longer exists.
pub async fn delete(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    confirm: Option<String>,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(dg::ServerError::Application)?;
    dg::confirmation::confirm_matches(confirm.as_deref(), &name)
        .map_err(dg::ServerError::Application)?;
    let result = request(
        server,
        key_path,
        Method::DELETE,
        &format!("/v1/branches/{name}"),
        None,
        dg::client::Semantics::Write,
    )
    .await?;
    let view_name = name.clone();
    finish(&result, json, move |result| {
        Outcome::with_facts(
            format!("deleted branch {view_name}"),
            vec![(
                "purges in".to_string(),
                dg::render::human_duration(result["purge_after_seconds"].as_u64().unwrap_or(0)),
            )],
        )
    })
}

/// Make one request against the gateway.
async fn request(
    server: &str,
    key_path: &Option<PathBuf>,
    method: Method,
    path: &str,
    body: Option<Value>,
    semantics: dg::client::Semantics,
) -> Result<Value, dg::ServerError> {
    let key = dg::client::read_key(key_path.as_deref().ok_or_else(|| {
        dg::ServerError::Application(Error::new(400, "missing_credential", "--key-file required"))
    })?)
    .map_err(dg::ServerError::Application)?;
    dg::client::request_with_semantics(server, &key, method, path, body, None, semantics)
        .await
        .map_err(dg::ServerError::Application)
}

/// Show the answer the way the operator asked for it.
///
/// A machine gets the API's JSON unchanged and a nonzero exit when the
/// answer is an error; a person gets the view, or the error the shell
/// draws for every other command.
fn finish(
    result: &Value,
    json: bool,
    view: impl FnOnce(&Value) -> Outcome,
) -> Result<(), dg::ServerError> {
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
    let mut out = std::io::stdout();
    view(result)
        .write(&mut out, &terminal_theme())
        .map_err(dg::ServerError::Application)
}

/// The error a failed API answer carries.
fn api_error() -> dg::ServerError {
    dg::ServerError::Application(Error::new(
        400,
        "api_error",
        "API request failed; see JSON response",
    ))
}

/// Where a branch came from, as a person reads it: the live parent, or
/// the backup set the snapshot came from.
fn source_of(branch: &Value) -> String {
    match branch["source"]["kind"].as_str() {
        Some("snapshot") => branch["source"]["set"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        _ => "live".to_string(),
    }
}
