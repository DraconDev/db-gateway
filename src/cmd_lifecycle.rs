//! Database lifecycle commands: clone, export, import, delete.

use super::{Request, human_result, show_error};
use dg::ServerError::Application;
use dg::error::Error;
use reqwest::Method;
use serde_json::json;
use std::io::IsTerminal;
use std::path::PathBuf;

pub async fn clone_db(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    source: String,
    destination: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&source).map_err(Application)?;
    dg::store::validate_name(&destination).map_err(Application)?;
    let view_source = source.clone();
    let view_destination = destination.clone();
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::POST,
            path: format!("/v1/databases/{source}/clone"),
            body: Some(json!({"destination":destination})),
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |result| {
            dg::render::Outcome::done(format!(
                "cloned {} to {}",
                result["source"].as_str().unwrap_or(&view_source),
                result["destination"].as_str().unwrap_or(&view_destination),
            ))
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{source}/clone"),
        Some(json!({"destination":destination})),
        None,
        dg::client::Semantics::Write,
    )))
}

pub async fn export(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    out: PathBuf,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    // The transfer itself lives in the library, so the CLI and the
    // terminal view perform the same download, the same hash check and
    // the same atomic publication.
    let report = match dg::transfer::export(server, &key, &name, &out).await {
        Ok(report) => report,
        Err(failure) => {
            if let Some(envelope) = &failure.envelope {
                show_error(envelope, json);
            }
            return Err(Application(failure.error));
        }
    };
    if json || !std::io::stdout().is_terminal() {
        let report = json!({
            "name": report.name,
            "out": report.out.display().to_string(),
            "size": report.size,
            "sha256": report.sha256,
        });
        println!("{report}");
        return Ok(());
    }
    // A person wants to know it worked and that the file is intact:
    // the size it is, and the hash to check it against.
    let mut stdout = std::io::stdout();
    dg::render::Outcome::with_facts(
        format!("exported {name}"),
        vec![
            ("file".to_string(), out.display().to_string()),
            ("size".to_string(), dg::render::human_bytes(report.size)),
            ("sha256".to_string(), report.sha256),
        ],
    )
    .write(&mut stdout, &dg::render::Theme::for_stream(true))
    .map_err(Application)?;
    Ok(())
}

pub async fn import(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    from: PathBuf,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    // The upload lives in the library for the same reason as the
    // download: one implementation of the chunking, the size ceiling
    // and the abort.
    let report = match dg::transfer::import(server, &key, &name, &from).await {
        Ok(report) => report,
        Err(failure) => {
            if let Some(envelope) = &failure.envelope {
                show_error(envelope, json);
            }
            return Err(Application(failure.error));
        }
    };
    if json || !std::io::stdout().is_terminal() {
        println!("{}", report.response);
    } else {
        let mut stdout = std::io::stdout();
        dg::render::Outcome::with_facts(
            format!("imported {name}"),
            vec![
                ("file".to_string(), from.display().to_string()),
                ("size".to_string(), dg::render::human_bytes(report.size)),
            ],
        )
        .write(&mut stdout, &dg::render::Theme::for_stream(true))
        .map_err(Application)?;
    }
    Ok(())
}

pub async fn delete(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    confirm: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    dg::confirmation::confirm_matches(confirm.as_deref(), &name).map_err(Application)?;
    let view_name = name.clone();
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method: Method::DELETE,
            path: format!("/v1/databases/{name}"),
            body: None,
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |result| {
            // A tombstone is not a deletion: the name is taken until
            // the retention window purges it, and the operator has to
            // know when it comes back.
            let purge_after = result["purge_after_seconds"]
                .as_u64()
                .map(dg::render::human_duration)
                .unwrap_or_else(|| "unknown".to_string());
            dg::render::Outcome::with_facts(
                format!("tombstoned {view_name}"),
                vec![("purges in".to_string(), purge_after)],
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
        format!("/v1/databases/{name}"),
        None,
        None,
        dg::client::Semantics::Write,
    )))
}

/// Bring a deleted database back inside its retention window.
pub async fn undelete(
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
            method: Method::POST,
            path: format!("/v1/databases/{name}/undelete"),
            body: None,
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |_result| {
            dg::render::Outcome::done(format!("restored {view_name}"))
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{name}/undelete"),
        None,
        None,
        dg::client::Semantics::Write,
    )))
}

/// Set or clear a database's delete protection.
pub async fn protect(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    off: bool,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let view_name = name.clone();
    let method = if off { Method::DELETE } else { Method::PUT };
    let path = format!("/v1/databases/{name}/protection");
    if human_result(
        super::Call {
            server,
            json,
            key_path,
            method,
            path: path.clone(),
            body: None,
            idempotency: None,
            semantics: dg::client::Semantics::Write,
        },
        Box::new(move |_result| {
            if off {
                dg::render::Outcome::done(format!("{view_name} is no longer protected"))
            } else {
                dg::render::Outcome::with_facts(
                    format!("{view_name} is protected"),
                    vec![(
                        "effect".to_string(),
                        "dg delete will refuse until protection is cleared".to_string(),
                    )],
                )
            }
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        method,
        path,
        None,
        None,
        dg::client::Semantics::Write,
    )))
}
