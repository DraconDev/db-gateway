//! Backup commands: live freshness (`status`, via the API) and the
//! offline `digest`/`verify` pair that read the backup root directly.

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::error::Error;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::PathBuf;

pub async fn status(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if let Some(backups) = table_json(server, json, key_path, Method::GET, "/v1/backups").await? {
        let mut rows = vec![
            json!(["backup_root", backups["backup_root"]]),
            json!(["stale_after_secs", backups["stale_after_secs"]]),
        ];
        if let Some(sets) = backups["sets"].as_array() {
            rows.push(json!([
                "sets",
                if sets.is_empty() {
                    "none".to_string()
                } else {
                    sets.iter()
                        .filter_map(|set| set["timestamp"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ]));
        }
        if let Some(per_database) = backups["per_database"].as_object() {
            let mut names: Vec<&String> = per_database.keys().collect();
            names.sort();
            for name in names {
                let fresh = &per_database[name.as_str()];
                rows.push(json!([
                    format!("backup {name}"),
                    format!(
                        "{}{}{}",
                        fresh["state"].as_str().unwrap_or("unknown"),
                        fresh["newest_set"]
                            .as_str()
                            .map(|set| format!(" @ {set}"))
                            .unwrap_or_default(),
                        fresh["age_secs"]
                            .as_u64()
                            .map(|age| format!(" ({age}s ago)"))
                            .unwrap_or_default()
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
    Ok(Some((Method::GET, "/v1/backups".into(), None, None)))
}

pub async fn digest(
    data_dir: PathBuf,
    backup_root: Option<PathBuf>,
    as_json: bool,
) -> Result<(), dg::ServerError> {
    // Deliberately local, not an HTTP round trip: the digest an
    // operator reads while the gateway is down or restarting is the
    // one they need most, and asking a wedged process for its own
    // health report is a bad way to deliver it. Same rule, same
    // `snapshot`, so this and the sink cannot disagree.
    let store = dg::store::Store::open(&data_dir).map_err(Application)?;
    let store = match backup_root {
        Some(root) => store.with_backup_root(Some(root)),
        None => store,
    };
    let health = dg::backup_health::snapshot(&store).map_err(Application)?;
    let note = dg::notify::digest_note(&health);
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&note).map_err(|_| Application(dg::error::Error::internal()))?
        );
    } else {
        let mut rows: Vec<Value> = vec![json!(["state", note.title.clone()])];
        for db in &health.databases {
            rows.push(json!([
                format!("backup {}", db.name),
                format!(
                    "{}{}{}",
                    db.state,
                    db.newest_set
                        .as_deref()
                        .map(|set| format!(" @ {set}"))
                        .unwrap_or_default(),
                    db.age_secs
                        .map(|age| format!(" ({age}s ago)"))
                        .unwrap_or_default()
                )
            ]));
        }
        if let Some(failure) = &health.last_failure {
            rows.push(json!([
                "last failure",
                format!(
                    "{} at step {}",
                    if failure.ts.is_empty() {
                        "?"
                    } else {
                        &failure.ts
                    },
                    if failure.step.is_empty() {
                        "?"
                    } else {
                        &failure.step
                    }
                )
            ]));
        }
        rows.push(json!(["action", note.action.clone()]));
        dg::shell::render(
            &json!({"columns":["property","value"],"rows":rows}),
            false,
            &mut std::io::stdout(),
        )
        .map_err(Application)?;
    }
    // Alert 4 is a condition, not a report: a digest that exits 0
    // while a database is unrepresented is how a schedule gets
    // mistaken for a backup. `unknown` fails too, because §2 says
    // unknown is the state that keeps a scheduled job honest.
    if health.attention().is_empty() {
        return Ok(());
    }
    Err(Application(dg::error::Error::new(
        1,
        "backup_unhealthy",
        "at least one database is stale or has no verified copy",
    )))
}

pub async fn verify(backup_root: PathBuf, set: String) -> Result<(), dg::ServerError> {
    let report = dg::backup::verify_set(&backup_root, &set).map_err(Application)?;
    println!(
        "{}",
        serde_json::to_value(&report).map_err(|_| Application(Error::internal()))?
    );
    if report.ok {
        return Ok(());
    }
    Err(Application(Error::new(
        400,
        "backup_verification_failed",
        "backup set failed verification; see JSON report",
    )))
}
