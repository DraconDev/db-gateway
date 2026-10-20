//! The `dg status` service summary.

use super::{Request, table_json};
use dg::render::{Align, Cell, Section, Style, Table, Theme};
use dg::ServerError::Application;
use reqwest::Method;
use serde_json::json;
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
        // A dashboard, grouped by what the reader is asking about: what
        // the service is, how it admits work, what it has been asked for,
        // and what each database is doing.
        let mut service = Section::new("service");
        service.field("version", Cell::styled(status["version"].as_str().unwrap_or(""), Style::Cyan));
        service.field(
            "uptime",
            Cell::text(
                status["uptime_secs"]
                    .as_u64()
                    .map(dg::render::human_duration)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );

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

        let mut requests = Section::new("requests");
        requests.field("total", Cell::number(status["requests"]["total"].as_u64().unwrap_or(0)));
        // A counter that is non-zero is the reason a reader is looking at
        // this view at all, so it is the one value that gets a colour.
        requests.field(
            "capacity exceeded",
            counter(status["requests"]["capacity_exceeded"].as_u64().unwrap_or(0)),
        );
        requests.field(
            "deadline exceeded",
            counter(status["requests"]["query_deadline_exceeded"].as_u64().unwrap_or(0)),
        );
        requests.field(
            "internal errors",
            counter(status["requests"]["internal_error"].as_u64().unwrap_or(0)),
        );

        let mut sections = vec![service, admission, requests];
        if let Some(usage) = status["databases"].as_object() {
            let mut names: Vec<&String> = usage.keys().collect();
            names.sort();
            if !names.is_empty() {
                let mut per_database = Section::new("per-database");
                let mut table = Table::new();
                table.column("database", Align::Left);
                table.column("read", Align::Right);
                table.column("written", Align::Right);
                table.column("waits", Align::Right);
                table.column("wait ms", Align::Right);
                table.column("timeouts", Align::Right);
                for name in names {
                    let entry = &usage[name.as_str()];
                    table.row(vec![
                        Cell::styled(name.clone(), Style::Cyan),
                        Cell::number(entry["rows_read"].as_u64().unwrap_or(0)),
                        Cell::number(entry["rows_written"].as_u64().unwrap_or(0)),
                        Cell::number(entry["queue_waits"].as_u64().unwrap_or(0)),
                        Cell::number(entry["queue_wait_ms"].as_u64().unwrap_or(0)),
                        Cell::number(entry["queue_timeouts"].as_u64().unwrap_or(0)),
                    ]);
                }
                per_database.table(table);
                sections.push(per_database);
            }
        }
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/status".into(), None, None)))
}

/// A counter that turns red once it is non-zero. The number is always
/// printed, so the colour is emphasis rather than the information.
fn counter(value: u64) -> Cell {
    if value > 0 {
        Cell::styled(value.to_string(), Style::Red)
    } else {
        Cell::number(value)
    }
}
