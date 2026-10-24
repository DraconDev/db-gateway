//! The three questions an operator asks about a database instead of
//! changing it: what SQLite would do with a statement, whether the file
//! is sound, how much space the file is wasting.
//!
//! A terminal gets the human view; a pipe or `--json` gets the API's
//! JSON unchanged, so a script reads the same numbers the server
//! reported.

use dg::ServerError::Application;
use dg::error::Error;
use dg::render::{Align, Cell, Outcome, Section, Style, Table, Theme};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// The credential this command talks with.
fn credential(key_path: &Option<PathBuf>) -> Result<String, dg::ServerError> {
    dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)
}

/// Whether the API answered with an error envelope.
fn api_failed(response: &Value) -> bool {
    response.get("error").is_some()
}

/// The error the CLI reports when the API refused.
fn api_error() -> Error {
    Error::new(400, "api_error", "API request failed; see JSON response")
}

/// Show what the API refused, and say the command failed.
///
/// The refusal is shown in the shape the caller asked for: JSON for a
/// pipe, the human error line for a terminal.
fn report_failure(response: &Value, json: bool) -> Result<(), dg::ServerError> {
    if json || !std::io::stdout().is_terminal() {
        println!("{response}");
        return Ok(());
    }
    dg::shell::render_with(response, false, &terminal_theme(), &mut std::io::stdout())
        .map_err(Application)
}

/// `dg explain <db> "<sql>"`: what SQLite would do with a statement.
///
/// The plan is asked for, not run: nothing is written, so a transport
/// failure costs a retry and nothing else.
pub async fn explain(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    sql: String,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = credential(key_path)?;
    let response = dg::client::request_with_semantics(
        server,
        &key,
        Method::POST,
        &format!("/v1/databases/{name}/explain"),
        Some(json!({"sql": sql})),
        None,
        dg::client::Semantics::Read,
    )
    .await
    .map_err(Application)?;
    if api_failed(&response) {
        report_failure(&response, json)?;
        return Err(Application(api_error()));
    }
    if json || !std::io::stdout().is_terminal() {
        println!("{response}");
        return Ok(());
    }
    let title = response["sql"].as_str().unwrap_or_default();
    let outcome = Outcome::list(format!("plan for {title}"), plan_table(&response));
    let mut out = std::io::stdout();
    outcome
        .write(&mut out, &terminal_theme())
        .map_err(Application)
}

/// `dg integrity <db>`: is the file sound.
///
/// The command exits non-zero when the file is not sound, so a deploy
/// gate can run it and trust the exit code.
pub async fn integrity(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = credential(key_path)?;
    let response = dg::client::request_with_semantics(
        server,
        &key,
        Method::GET,
        &format!("/v1/databases/{name}/integrity"),
        None,
        None,
        dg::client::Semantics::Read,
    )
    .await
    .map_err(Application)?;
    if api_failed(&response) {
        report_failure(&response, json)?;
        return Err(Application(api_error()));
    }
    let sound = response["ok"].as_bool().unwrap_or(false);
    if json || !std::io::stdout().is_terminal() {
        println!("{response}");
    } else {
        let mut out = std::io::stdout();
        write_integrity(&mut out, &response, sound)?;
    }
    if sound {
        return Ok(());
    }
    Err(Application(Error::new(
        400,
        "integrity_check_failed",
        "the database is not sound; see the report",
    )))
}

/// `dg vacuum <db>`: reclaim the space the file is wasting.
pub async fn vacuum(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = credential(key_path)?;
    // A vacuum rewrites the file, so a transport failure leaves the
    // outcome genuinely in doubt: it is reported as a write.
    let response = dg::client::request_with_semantics(
        server,
        &key,
        Method::POST,
        &format!("/v1/databases/{name}/vacuum"),
        None,
        None,
        dg::client::Semantics::Write,
    )
    .await
    .map_err(Application)?;
    if api_failed(&response) {
        report_failure(&response, json)?;
        return Err(Application(api_error()));
    }
    if json || !std::io::stdout().is_terminal() {
        println!("{response}");
        return Ok(());
    }
    let mut out = std::io::stdout();
    write_vacuum(&mut out, &response)
}

/// The plan as a table: the step, the step it belongs to, what SQLite
/// said, and what that means.
///
/// The rows are indented by their place in the plan's tree, so the
/// answer reads as the shape SQLite chose rather than as a flat list.
pub fn plan_table(response: &Value) -> Table {
    let plan = response["plan"].as_array().cloned().unwrap_or_default();
    let depths = plan_depths(&plan);
    let mut table = Table::new();
    table.column("id", Align::Right);
    table.column("parent", Align::Right);
    table.column("detail", Align::Left);
    table.column("meaning", Align::Left);
    for (row, depth) in plan.iter().zip(depths) {
        let detail = row["detail"].as_str().unwrap_or_default();
        table.row(vec![
            Cell::number(row["id"].as_i64().unwrap_or_default()),
            Cell::number(row["parent"].as_i64().unwrap_or_default()),
            Cell::text(format!("{}{detail}", "  ".repeat(depth))),
            Cell::text(row["meaning"].as_str().unwrap_or_default()),
        ]);
    }
    if !plan.is_empty() {
        table.footer(format!("{} step(s)", plan.len()));
    }
    table
}

/// How deep each step sits in the plan's tree.
///
/// SQLite's plan rows name their parent, so the shape of the tree is
/// in the answer already; the depth is what turns those rows back into
/// a list a person can read.
fn plan_depths(plan: &[Value]) -> Vec<usize> {
    let mut depths = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(plan.len());
    for row in plan {
        let id = row["id"].as_i64().unwrap_or_default();
        let parent = row["parent"].as_i64().unwrap_or_default();
        let depth = depths.get(&parent).map(|depth| depth + 1).unwrap_or(0);
        depths.insert(id, depth);
        out.push(depth);
    }
    out
}

/// The problems the integrity report found.
///
/// The messages are SQLite's own, so they are not rewritten: the words
/// are the diagnosis, and a paraphrase of them would lose it.
pub fn integrity_problems(response: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for message in response["integrity"].as_array().into_iter().flatten() {
        if let Some(text) = message.as_str()
            && !text.eq_ignore_ascii_case("ok")
        {
            problems.push(text.to_string());
        }
    }
    if let Some(failure) = response["failure"].as_str() {
        problems.push(failure.to_string());
    }
    for violation in response["foreign_keys"].as_array().into_iter().flatten() {
        let table = violation["table"].as_str().unwrap_or("?");
        let parent = violation["parent"].as_str().unwrap_or("?");
        match violation["rowid"].as_i64() {
            Some(rowid) => problems.push(format!(
                "foreign key violation: {table} row {rowid} references {parent}"
            )),
            None => problems.push(format!("foreign key mismatch: {table} references {parent}")),
        }
    }
    problems
}

/// The facts behind the verdict.
pub fn integrity_facts(response: &Value) -> Vec<(String, String)> {
    let violations = response["foreign_key_count"].as_u64().unwrap_or(0);
    let mut facts = vec![(
        "foreign keys".to_string(),
        if violations == 0 {
            "no violations".to_string()
        } else {
            format!("{violations} violation(s)")
        },
    )];
    if response["truncated"].as_bool().unwrap_or(false) {
        facts.push((
            "note".to_string(),
            "the list of problems was cut short; the counts are complete".to_string(),
        ));
    }
    if !response["complete"].as_bool().unwrap_or(true) {
        facts.push((
            "note".to_string(),
            "the check did not finish; the file is not cleared".to_string(),
        ));
    }
    facts.push((
        "duration".to_string(),
        format!("{} ms", response["duration_ms"].as_u64().unwrap_or(0)),
    ));
    facts
}

/// Draw the integrity report.
pub fn write_integrity(
    out: &mut dyn Write,
    response: &Value,
    sound: bool,
) -> Result<(), dg::ServerError> {
    let name = response["database"].as_str().unwrap_or_default();
    let mut section = Section::new("");
    section.field(
        "",
        Cell::styled(
            if sound {
                format!("{name} is sound")
            } else {
                format!("{name} is not sound")
            },
            if sound { Style::Green } else { Style::Red },
        ),
    );
    for (key, value) in integrity_facts(response) {
        section.field(&key, Cell::text(value));
    }
    let mut sections = vec![section];
    let problems = integrity_problems(response);
    if !problems.is_empty() {
        let mut list = Section::new("problems");
        for problem in problems {
            list.field("-", Cell::styled(problem, Style::Red));
        }
        sections.push(list);
    }
    dg::render::write_sections(out, &terminal_theme(), &sections).map_err(Application)
}

/// Draw the vacuum report.
pub fn write_vacuum(out: &mut dyn Write, response: &Value) -> Result<(), dg::ServerError> {
    let name = response["database"].as_str().unwrap_or_default();
    let reclaimed = response["reclaimed_bytes"].as_u64().unwrap_or(0);
    let mut facts = vec![
        (
            "size before".to_string(),
            dg::render::human_bytes(response["size_before_bytes"].as_u64().unwrap_or(0)),
        ),
        (
            "size after".to_string(),
            dg::render::human_bytes(response["size_after_bytes"].as_u64().unwrap_or(0)),
        ),
        (
            // A vacuum that reclaimed nothing is said plainly: a saving
            // that did not happen is worse than no saving.
            "reclaimed".to_string(),
            if reclaimed == 0 {
                "nothing to reclaim".to_string()
            } else {
                dg::render::human_bytes(reclaimed)
            },
        ),
        (
            "duration".to_string(),
            format!("{} ms", response["duration_ms"].as_u64().unwrap_or(0)),
        ),
    ];
    if !response["checkpoint_complete"].as_bool().unwrap_or(true) {
        facts.push((
            "checkpoint".to_string(),
            "incomplete; the file may still be wasting the space".to_string(),
        ));
    }
    let outcome = Outcome::with_facts(format!("vacuumed {name}"), facts);
    outcome.write(out, &terminal_theme()).map_err(Application)
}
