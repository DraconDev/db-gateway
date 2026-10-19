//! Credential administration commands (owner only at the API).

use super::{Cli, Request};
use dg::ServerError::Application;
use reqwest::Method;
use serde_json::json;
use std::path::PathBuf;

pub async fn create(name: String, role: String, database: Vec<String>) -> Result<Option<Request>, dg::ServerError> {
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

pub async fn list(cli: &Cli, key_path: &Option<PathBuf>) -> Result<Option<Request>, dg::ServerError> {
    if let Some(keys) = table_json(&cli, &key_path, Method::GET, "/v1/keys").await? {
        let rows: Vec<Value> = keys["keys"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|key| {
                let databases = key["databases"]
                    .as_array()
                    .map(|dbs| {
                        dbs.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                json!([
                    key["name"],
                    key["role"],
                    if databases.is_empty() && key["role"].as_str() == Some("owner") {
                        "all".to_string()
                    } else {
                        databases
                    }
                ])
            })
            .collect();
        dg::shell::render(
            &json!({"columns":["name","role","databases"],"rows":rows}),
            false,
            &mut std::io::stdout(),
        )
        .map_err(Application)?;
        return Ok(None);
    }
    Ok(Some((Method::GET, "/v1/keys".into(), None, None)))
}

pub async fn revoke(name: String) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    Ok(Some((Method::DELETE, format!("/v1/keys/{name}"), None, None)))
}

pub async fn rotate_owner() -> Result<Option<Request>, dg::ServerError> {
    Ok(Some((Method::POST, "/v1/keys/owner/rotate".into(), None, None)))
}
