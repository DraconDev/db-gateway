//! `dg diff`: what differs between two databases.
//!
//! WHY the command is a request and not a local comparison: the two
//! databases live on the gateway's host, and the answer is a fact about
//! two files there. Nothing about the comparison needs the rows to
//! travel, so the gateway compares them in place and sends back what it
//! found.
//!
//! WHY the exit code matters: "are these two databases the same?" is a
//! question a script asks before it does something else. The command
//! therefore exits non-zero when the databases are not the same -- and
//! also when the comparison could not be finished, because a comparison
//! that did not finish has not found them equal.
//!
//! The report is the API's report: a terminal gets the human view of
//! it, a pipe or `--json` gets the JSON unchanged.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use dg::ServerError::Application;
use dg::error::Error;
use dg::render::{Align, Cell, Section, Style, Table, Theme};

/// The report, as the API sends it.
///
/// The shape is spelled out here rather than borrowed from the library's
/// own diff module: this file is compiled both into the binary and into
/// the test crate, and it must not depend on the library's wiring to
/// exist. The test asserts that both agree.
///
/// The type is public so the test crate can name it and assert the human
/// view against the same shape the API sends.
#[derive(Debug, serde::Deserialize)]
pub struct Report {
    /// The database on the left of the comparison.
    left: String,
    /// The database on the right of the comparison.
    right: String,
    /// Whether the two databases are the same.
    identical: bool,
    /// How many rows were compared.
    compared_rows: u64,
    /// Whether the comparison hit the row cap.
    truncated: bool,
    /// How the two schemas differ.
    schema: Schema,
    /// How the rows of every shared table compare.
    tables: Vec<TableView>,
}

/// How the two schemas differ.
#[derive(Debug, serde::Deserialize)]
pub struct Schema {
    /// Whether the two schemas are the same.
    identical: bool,
    /// The objects only the left database has.
    only_left: Vec<Object>,
    /// The objects only the right database has.
    only_right: Vec<Object>,
    /// The objects both have, but not the same.
    changed: Vec<Change>,
}

/// One schema object.
#[derive(Debug, serde::Deserialize)]
pub struct Object {
    /// The object's name.
    name: String,
    /// What kind of object it is.
    kind: String,
}

/// One schema object that exists on both sides and does not match.
#[derive(Debug, serde::Deserialize)]
pub struct Change {
    /// The object's name.
    name: String,
    /// What kind of object it is.
    kind: String,
    /// What differs.
    detail: String,
}

/// How one table's rows compare.
///
/// Named `TableView` and not `Table`: the renderer owns a `Table` of its
/// own, and two meanings of one name in one file is a bug waiting.
#[derive(Debug, serde::Deserialize)]
pub struct TableView {
    /// The table's name.
    name: String,
    /// What the comparison established.
    status: String,
    /// How many rows the left database holds.
    left_rows: Option<u64>,
    /// How many rows the right database holds.
    right_rows: Option<u64>,
    /// How many rows were compared.
    compared_rows: u64,
    /// Whether the comparison stopped at the row cap.
    truncated: bool,
    /// How many rows differ, when the rows were compared.
    differing_rows: Option<u64>,
    /// Why the rows were not compared, when they were not.
    reason: Option<String>,
}

/// The status word for a table whose rows are the same.
const EQUAL: &str = "equal";
/// The status word for a table whose rows differ.
const DIFFERENT: &str = "different";
/// The status word for a table whose rows were not compared.
const NOT_COMPARED: &str = "not_compared";

/// `dg diff <left> <right>`.
///
/// A terminal gets the human report; a pipe or `--json` gets the API's
/// JSON unchanged. The command exits non-zero when the databases are not
/// the same, and also when the comparison could not be finished.
pub async fn diff(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    left: &str,
    right: &str,
    all: bool,
) -> Result<(), dg::ServerError> {
    let key = dg::client::read_key(key_path.as_deref().ok_or_else(|| {
        Application(Error::new(400, "missing_credential", "--key-file required"))
    })?)
    .map_err(Application)?;
    let body = serde_json::json!({"left": left, "right": right});
    // A diff is a read: the gateway compares the two databases itself
    // and commits nothing.
    let response = dg::client::request_with_semantics(
        server,
        &key,
        reqwest::Method::POST,
        "/v1/diff",
        Some(body),
        None,
        dg::client::Semantics::Read,
    )
    .await
    .map_err(Application)?;
    if response.get("error").is_some() {
        // The API's refusal is the answer; it is shown as it came.
        if json || !std::io::stdout().is_terminal() {
            println!("{response}");
        } else {
            let theme = Theme::for_stream(true);
            dg::shell::render_with(&response, false, &theme, &mut std::io::stdout())
                .map_err(Application)?;
        }
        return Err(Application(Error::new(
            400,
            "api_error",
            "the diff request was refused; see the response",
        )));
    }
    let report: Report =
        serde_json::from_value(response.clone()).map_err(|_| Application(Error::internal()))?;
    if json || !std::io::stdout().is_terminal() {
        // The machine surface is the API's JSON, unchanged.
        println!("{response}");
    } else {
        let mut out = std::io::stdout();
        dg::render::write_sections(
            &mut out,
            &Theme::for_stream(true),
            &human_view(&report, all),
        )
        .map_err(Application)?;
        out.flush().map_err(|_| Application(Error::internal()))?;
    }
    if report.identical {
        return Ok(());
    }
    // The verdict is the exit code: a script asks "are these the same?"
    // and reads the status, not the report.
    let incomplete = report
        .tables
        .iter()
        .any(|table| table.status == NOT_COMPARED);
    let message = if incomplete {
        "the databases differ, and at least one table was not compared; see the report"
    } else {
        "the databases differ; see the report"
    };
    Err(Application(Error::with_message(
        1,
        "databases_differ",
        message.to_string(),
    )))
}

/// The human view of a report.
///
/// The verdict comes first, then the schema differences, then the tables.
/// Only the tables that differ are shown unless `all` is asked for: a
/// report of two hundred equal tables buries the one that is not.
pub fn human_view(report: &Report, all: bool) -> Vec<Section> {
    let mut sections = Vec::new();

    let incomplete = report
        .tables
        .iter()
        .any(|table| table.status == NOT_COMPARED);
    let headline = if report.identical {
        format!("{} and {} are identical", report.left, report.right)
    } else if incomplete {
        format!(
            "{} and {} differ, and the comparison was not complete",
            report.left, report.right
        )
    } else {
        format!("{} and {} differ", report.left, report.right)
    };
    let mut verdict = Section::new(headline);
    verdict.field("rows compared", Cell::number(report.compared_rows));
    if report.truncated {
        verdict.field(
            "comparison",
            Cell::styled("truncated: not every row was compared", Style::Yellow),
        );
    }
    sections.push(verdict);

    let mut schema = Section::new("schema");
    if report.schema.identical {
        schema.field("difference", Cell::text("none"));
    } else {
        if !report.schema.only_left.is_empty() {
            schema.field(
                format!("only in {}", report.left),
                Cell::text(describe_objects(&report.schema.only_left)),
            );
        }
        if !report.schema.only_right.is_empty() {
            schema.field(
                format!("only in {}", report.right),
                Cell::text(describe_objects(&report.schema.only_right)),
            );
        }
        if !report.schema.changed.is_empty() {
            let changes: Vec<String> = report
                .schema
                .changed
                .iter()
                .map(|change| format!("{} ({}): {}", change.name, change.kind, change.detail))
                .collect();
            schema.field("changed", Cell::text(changes.join("; ")));
        }
    }
    sections.push(schema);

    let shown: Vec<&TableView> = if all {
        report.tables.iter().collect()
    } else {
        report
            .tables
            .iter()
            .filter(|table| table.status != EQUAL)
            .collect()
    };
    if !shown.is_empty() {
        let mut table = Table::new();
        table.column("table", Align::Left);
        table.column("status", Align::Left);
        table.column(format!("{} rows", report.left), Align::Right);
        table.column(format!("{} rows", report.right), Align::Right);
        table.column("compared", Align::Right);
        table.column("note", Align::Left);
        for row in shown {
            table.row(vec![
                Cell::styled(row.name.clone(), Style::Cyan),
                Cell::styled(row.status.clone(), status_style(&row.status, row.truncated)),
                row.left_rows.map(Cell::number).unwrap_or(Cell::text("-")),
                row.right_rows.map(Cell::number).unwrap_or(Cell::text("-")),
                Cell::number(row.compared_rows),
                Cell::text(note(row)),
            ]);
        }
        let mut rows = Section::new("rows");
        rows.table(table);
        sections.push(rows);
    }
    sections
}

/// The objects of one side, as a list an operator reads.
fn describe_objects(objects: &[Object]) -> String {
    objects
        .iter()
        .map(|object| format!("{} ({})", object.name, object.kind))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The note under a table: why it was not compared, or how many rows differ.
fn note(row: &TableView) -> String {
    let mut note = row.reason.clone().unwrap_or_default();
    let differing = row.differing_rows.unwrap_or(0);
    if differing > 0 {
        let separator = if note.is_empty() { "" } else { "; " };
        note.push_str(separator);
        note.push_str(&format!("{differing} rows differ"));
    }
    if row.truncated && note.is_empty() {
        note.push_str("the comparison stopped at the row cap");
    }
    note
}

/// The style of a status word.
///
/// The word itself carries the meaning; the colour only repeats it.
fn status_style(status: &str, truncated: bool) -> Style {
    if truncated {
        return Style::Yellow;
    }
    match status {
        EQUAL => Style::Green,
        DIFFERENT => Style::Red,
        _ => Style::Yellow,
    }
}
