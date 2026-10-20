//! The `dg status` service summary, and the `dg watch` snapshot of it.

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::render::{
    Align, Cell, Section, Style, Table, Theme, human_count, human_duration, human_micros,
    human_percent, human_rate,
};
use reqwest::Method;
use serde_json::Value;
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
        dg::render::write_sections(&mut out, &terminal_theme(), &dashboard(&status)).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/status".into(), None, None)))
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

/// The service dashboard, grouped by what the reader is asking about.
///
/// The questions an operator has are "is it up", "is it busy", "is it
/// healthy" and "which of my databases is doing the work". The
/// dashboard answers those in that order, rather than listing the
/// server's properties.
pub fn dashboard(status: &Value) -> Vec<Section> {
    let mut sections = Vec::new();

    // What the service is, and whether it is up.
    let mut service = Section::new("service");
    service.field("version", Cell::text(status["version"].as_str().unwrap_or("?")));
    service.field(
        "uptime",
        Cell::text(
            status["uptime_secs"]
                .as_u64()
                .map(human_duration)
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    );
    let live = status["database_counts"]["live"].as_u64().unwrap_or(0);
    let tombstoned = status["database_counts"]["tombstoned"].as_u64().unwrap_or(0);
    service.field(
        "databases",
        Cell::text(format!(
            "{live} live, {tombstoned} tombstoned"
        )),
    );
    sections.push(service);

    // How much work it is doing, and how fast.
    let requests = &status["requests"];
    let total = requests["total"].as_u64().unwrap_or(0);
    let errors = requests["capacity_exceeded"].as_u64().unwrap_or(0)
        + requests["query_deadline_exceeded"].as_u64().unwrap_or(0)
        + requests["internal_error"].as_u64().unwrap_or(0);
    let mut activity = Section::new("activity");
    activity.field("requests", Cell::text(human_count(total, "")));
    let span = requests["recent_span_secs"].as_u64().unwrap_or(1);
    let recent: u64 = requests["recent_per_second"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_u64).sum())
        .unwrap_or(0);
    activity.field("rate", Cell::text(human_rate(recent, span)));
    activity.field("errors", Cell::text(human_count(errors, "")));
    activity.field("error rate", Cell::text(human_percent(errors, total)));
    // The recent shape of the traffic, as a chart in one line.
    if let Some(values) = requests["recent_per_second"].as_array() {
        let counts: Vec<u64> = values.iter().filter_map(Value::as_u64).collect();
        if !counts.is_empty() {
            activity.field("recent", Cell::Sparkline(counts));
        }
    }
    sections.push(activity);

    // Whether the admission queue is hurting anyone.
    let mut admission = Section::new("admission");
    admission.field(
        "acquire timeout",
        Cell::text(
            status["admission"]["acquire_timeout_ms"]
                .as_u64()
                .map(|ms| format!("{ms} ms"))
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    );
    admission.field(
        "read permits",
        Cell::number(status["admission"]["read_permits"].as_u64().unwrap_or(0)),
    );
    admission.field(
        "write permits",
        Cell::number(status["admission"]["write_permits"].as_u64().unwrap_or(0)),
    );
    match (
        requests["wait_p50_us"].as_u64(),
        requests["wait_p95_us"].as_u64(),
    ) {
        (Some(p50), Some(p95)) => {
            admission.field("wait p50", Cell::text(human_micros(p50)));
            admission.field("wait p95", Cell::text(human_micros(p95)));
        }
        // No wait has been recorded: the queue has never contended, and
        // saying "0 ms" would claim a measurement that was never made.
        _ => {
            admission.field("wait p50", Cell::text("no waits recorded"));
            admission.field("wait p95", Cell::text("no waits recorded"));
        }
    }
    sections.push(admission);

    // Which database is doing the work.
    if let Some(usage) = status["databases"].as_object()
        && !usage.is_empty()
    {
        let mut per_database = Section::new("per-database");
        let mut table = Table::new();
        table.column("database", Align::Left);
        table.column("rows read", Align::Right);
        table.column("rows written", Align::Right);
        table.column("waits", Align::Right);
        table.column("wait p95", Align::Right);
        table.column("last write", Align::Left);
        for (name, entry) in usage {
            let waits = entry["queue_waits"].as_u64().unwrap_or(0);
            let wait_p95 = match (waits, entry["queue_wait_max_us"].as_u64()) {
                (0, _) => Cell::text("-"),
                (_, Some(max)) => Cell::text(human_micros(max)),
                (_, None) => Cell::text("-"),
            };
            let last_write = match entry["last_write"].as_u64() {
                Some(epoch) => Cell::text(human_epoch_ago(epoch)),
                None => Cell::text("never"),
            };
            table.row(vec![
                Cell::styled(name.clone(), Style::Cyan),
                Cell::number(entry["rows_read"].as_u64().unwrap_or(0)),
                Cell::number(entry["rows_written"].as_u64().unwrap_or(0)),
                Cell::number(waits),
                wait_p95,
                last_write,
            ]);
        }
        per_database.table(table);
        sections.push(per_database);
    }

    sections
}

/// How long ago a timestamp was, as a person reads it.
///
/// The absolute time is kept alongside, because "3m ago" alone does not
/// say when a write happened, only how recently.
fn human_epoch_ago(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch);
    let age = now.saturating_sub(epoch);
    format!("{} ago", human_duration(age))
}
