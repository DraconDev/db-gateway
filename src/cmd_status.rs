//! The `dg status` service summary.

use super::{Cli, Request};
use dg::ServerError::Application;
use reqwest::Method;
use serde_json::json;
use std::path::PathBuf;

pub async fn status(cli: &Cli, key_path: &Option<PathBuf>) -> Result<Option<Request>, dg::ServerError> {
    if let Some(status) = table_json(&cli, &key_path, Method::GET, "/v1/status").await? {
        let mut rows = vec![
            json!(["version", status["version"]]),
            json!(["uptime_secs", status["uptime_secs"]]),
            json!([
                "acquire_timeout_ms",
                status["admission"]["acquire_timeout_ms"]
            ]),
            json!(["read_permits", status["admission"]["read_permits"]]),
            json!(["write_permits", status["admission"]["write_permits"]]),
            json!(["requests_total", status["requests"]["total"]]),
            json!(["capacity_exceeded", status["requests"]["capacity_exceeded"]]),
            json!([
                "deadline_exceeded",
                status["requests"]["query_deadline_exceeded"]
            ]),
            json!(["internal_error", status["requests"]["internal_error"]]),
        ];
        if let Some(usage) = status["databases"].as_object() {
            let mut names: Vec<&String> = usage.keys().collect();
            names.sort();
            for name in names {
                let entry = &usage[name.as_str()];
                rows.push(json!([
                    format!("usage {name}"),
                    format!(
                        "read {}, written {}, queue {} waits/{}ms/{} timeouts",
                        entry["rows_read"].as_u64().unwrap_or(0),
                        entry["rows_written"].as_u64().unwrap_or(0),
                        entry["queue_waits"].as_u64().unwrap_or(0),
                        entry["queue_wait_ms"].as_u64().unwrap_or(0),
                        entry["queue_timeouts"].as_u64().unwrap_or(0),
                    )
                ]));
            }
        }
        dg::shell::render(
            &json!({"columns":["property","value"],"rows":rows}),
            false,
            &mut std::io::stdout(),
        )
        .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/status".into(), None, None)))
}
