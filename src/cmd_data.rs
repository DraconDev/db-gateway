//! Everyday data commands against the API: list/info/create,
//! query/batch, and the interactive shell.

use super::{Cli, Request};
use dg::ServerError::Application;
use dg::error::Error;
use reqwest::Method;
use serde_json::json;
use std::path::PathBuf;

pub async fn list(cli: &Cli, key_path: &Option<PathBuf>) -> Result<Option<Request>, dg::ServerError> {
    // Interactive terminals get an aligned table; piped output
    // and --json stay machine-readable JSON.
    if !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
            Error::new(400, "missing_credential", "--key-file required"),
        ))?)
        .map_err(Application)?;
        let result = dg::client::request(
            &cli.server,
            &key,
            Method::GET,
            "/v1/databases",
            None,
            None,
        )
        .await
        .map_err(Application)?;
        if api_failed(&result) {
            dg::shell::render(&result, false, &mut std::io::stdout())
                .map_err(Application)?;
            return Err(Application(api_error()));
        }
        let rows: Vec<Value> = result["databases"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|name| json!([name]))
            .collect();
        dg::shell::render(
            &json!({"columns":["name"],"rows":rows}),
            false,
            &mut std::io::stdout(),
        )
        .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/databases".into(), None, None)))
}

pub async fn info(cli: &Cli, key_path: &Option<PathBuf>, name: String) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    if !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
            Error::new(400, "missing_credential", "--key-file required"),
        ))?)
        .map_err(Application)?;
        let info = dg::client::request(
            &cli.server,
            &key,
            Method::GET,
            &format!("/v1/databases/{name}"),
            None,
            None,
        )
        .await
        .map_err(Application)?;
        if api_failed(&info) {
            dg::shell::render(&info, false, &mut std::io::stdout()).map_err(Application)?;
            return Err(Application(api_error()));
        }
        // Usage is auxiliary: a failed status fetch degrades the
        // two usage cells instead of failing the command. Absent
        // from a good status means true zero activity.
        let (rows_read, rows_written) =
            dg::client::request(&cli.server, &key, Method::GET, "/v1/status", None, None)
                .await
                .ok()
                .filter(|status| !api_failed(status))
                .map(|status| {
                    let use_ = &status["databases"][&name];
                    (
                        use_["rows_read"].as_u64().unwrap_or(0).to_string(),
                        use_["rows_written"].as_u64().unwrap_or(0).to_string(),
                    )
                })
                .unwrap_or_else(|| ("unavailable".to_string(), "unavailable".to_string()));
        let mut rows = vec![
            json!(["name", info["name"]]),
            json!(["tombstoned", info["tombstoned"]]),
            json!(["size_bytes", info["size_bytes"]]),
            json!(["rows_read", rows_read]),
            json!(["rows_written", rows_written]),
        ];
        if info["tombstoned"].as_bool() == Some(true) {
            rows.push(json!(["purge_after", info["purge_after"]]));
        } else {
            rows.push(json!(["created_at", info["created_at"]]));
            let tables = info["tables"]
                .as_array()
                .map(|tables| {
                    tables
                        .iter()
                        .map(|t| {
                            format!(
                                "{} ({} rows)",
                                t["name"].as_str().unwrap_or("?"),
                                t["rows"].as_u64().unwrap_or(0)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            rows.push(json!(["tables", tables]));
        }
        dg::shell::render(
            &json!({"columns":["property","value"],"rows":rows}),
            false,
            &mut std::io::stdout(),
        )
        .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, format!("/v1/databases/{name}"), None, None)))
}

pub async fn create(name: String) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    Ok(Some((
        Method::POST,
        "/v1/databases".into(),
        Some(json!({"name":name})),
        None,
    )))
}

pub async fn shell(cli: &Cli, key_path: &Option<PathBuf>, name: Option<String>) -> Result<(), dg::ServerError> {
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    if let Some(name) = &name {
        dg::store::validate_name(name).map_err(Application)?;
    }
    return dg::shell::run(
        &cli.server,
        &key,
        cli.json,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        name.as_deref(),
        std::io::stdin(),
        std::io::stdout(),
    )
    .await
    .map_err(Application);
}

pub async fn query(name: String, sql: String, args: String, idempotency_key: Option<String>) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let args: Vec<Value> =
        serde_json::from_str(&args).map_err(|_| Application(Error::invalid()))?;
    // Interactive terminals get the shell's aligned table; piped
    // output and --json stay machine-readable JSON.
    if !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
            Error::new(400, "missing_credential", "--key-file required"),
        ))?)
        .map_err(Application)?;
        let result = dg::client::request(
            &cli.server,
            &key,
            Method::POST,
            &format!("/v1/databases/{name}/query"),
            Some(json!({"sql":sql,"args":args})),
            idempotency_key.as_deref(),
        )
        .await
        .map_err(Application)?;
        dg::shell::render(&result, false, &mut std::io::stdout()).map_err(Application)?;
        if result.get("error").is_some() {
            return Err(Application(Error::new(
                400,
                "api_error",
                "API request failed; see response above",
            )));
        }
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/query"),
        Some(json!({"sql":sql,"args":args})),
        idempotency_key,
    )))
}

pub async fn batch(name: String, file: PathBuf, idempotency_key: Option<String>) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    // Open once with O_NONBLOCK so a writer-less FIFO cannot make
    // the CLI hang before the descriptor is checked. The metadata
    // check is on that same descriptor, so a path replacement
    // cannot turn this into a read of a different file.
    #[cfg(unix)]
    let batch_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&file);
    #[cfg(not(unix))]
    let batch_file = OpenOptions::new().read(true).open(&file);
    let mut batch_file = batch_file.map_err(|_| Application(Error::invalid()))?;
    if !batch_file
        .metadata()
        .map_err(|_| Application(Error::invalid()))?
        .is_file()
    {
        return Err(Application(Error::invalid()));
    }
    let mut batch_bytes = Vec::new();
    Read::by_ref(&mut batch_file)
        .take(65537)
        .read_to_end(&mut batch_bytes)
        .map_err(|_| Application(Error::invalid()))?;
    if batch_bytes.len() > 65536 {
        return Err(Application(Error::new(
            413,
            "input_limit_exceeded",
            "batch input exceeds 64 KiB",
        )));
    }
    let body: dg::execution::Batch =
        serde_json::from_slice(&batch_bytes).map_err(|_| Application(Error::invalid()))?;
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/batch"),
        Some(serde_json::to_value(body).map_err(|_| Application(Error::invalid()))?),
        idempotency_key,
    )))
}
