//! Backup commands: live freshness (`status`, via the API) and the
//! offline `digest`/`verify` pair that read the backup root directly.

use super::{Request, table_json};
use dg::ServerError::Application;
use dg::error::Error;
use dg::render::{Align, Cell, Section, Style, Table, Theme};
use reqwest::Method;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> Theme {
    Theme::for_stream(std::io::stdout().is_terminal())
}

/// The style for a backup freshness state. The state word itself is
/// always spelled out, so the colour is a second rendering of the same
/// information rather than the only rendering of it.
fn state_style(state: &str) -> Style {
    match state {
        "fresh" => Style::Green,
        "stale" => Style::Yellow,
        _ => Style::Red,
    }
}

/// The style for a notification's severity.
fn severity_style(severity: &dg::notify::Severity) -> Style {
    match severity {
        dg::notify::Severity::Alert => Style::Red,
        dg::notify::Severity::Digest => Style::Green,
    }
}

pub async fn status(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
) -> Result<Option<Request>, dg::ServerError> {
    if let Some(backups) = table_json(server, json, key_path, Method::GET, "/v1/backups").await? {
        let mut root = Section::new("backup");
        root.field(
            "root",
            Cell::styled(
                backups["backup_root"]
                    .as_str()
                    .unwrap_or("(none configured)"),
                Style::Cyan,
            ),
        );
        root.field(
            "stale after",
            Cell::text(
                backups["stale_after_secs"]
                    .as_u64()
                    .map(dg::render::human_duration)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        let sets = backups["sets"].as_array().cloned().unwrap_or_default();
        root.field("sets", Cell::number(sets.len()));

        let mut sections = vec![root];

        if !sets.is_empty() {
            let mut published = Section::new("published sets");
            let mut table = Table::new();
            table.column("set", Align::Left);
            table.column("coverage from", Align::Left);
            table.column("databases", Align::Right);
            table.column("registry", Align::Left);
            for set in &sets {
                table.row(vec![
                    // The set keeps its raw stamp: that is the exact
                    // argument `dg backup verify --set` takes, so
                    // rewriting it would make the two disagree.
                    Cell::text(set["timestamp"].as_str().unwrap_or("?")),
                    Cell::text(
                        set["coverage_start"]
                            .as_i64()
                            .map(dg::render::human_epoch)
                            .unwrap_or_else(|| "unknown".to_string()),
                    ),
                    Cell::number(
                        set["databases"]
                            .as_array()
                            .map(|dbs| dbs.len())
                            .unwrap_or(0),
                    ),
                    Cell::text(if set["has_registry"].as_bool() == Some(true) {
                        "yes"
                    } else {
                        "no"
                    }),
                ]);
            }
            published.table(table);
            sections.push(published);
        }

        if let Some(per_database) = backups["per_database"].as_object() {
            let mut names: Vec<&String> = per_database.keys().collect();
            names.sort();
            if !names.is_empty() {
                let mut freshness = Section::new("per-database");
                let mut table = Table::new();
                table.column("database", Align::Left);
                table.column("state", Align::Left);
                table.column("newest set", Align::Left);
                table.column("age", Align::Right);
                for name in names {
                    let fresh = &per_database[name.as_str()];
                    let state = fresh["state"].as_str().unwrap_or("unknown");
                    table.row(vec![
                        Cell::styled(name.clone(), Style::Cyan),
                        Cell::styled(state, state_style(state)),
                        Cell::text(
                            fresh["newest_set"]
                                .as_str()
                                .map(dg::render::human_set_stamp)
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                        Cell::text(
                            fresh["age_secs"]
                                .as_u64()
                                .map(dg::render::human_duration)
                                .unwrap_or_else(|| "unknown".to_string()),
                        ),
                    ]);
                }
                freshness.table(table);
                sections.push(freshness);
            }
        }
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
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
    // The digest is a REPORT, not a command with a machine form: the
    // scheduled unit runs it and its output lands in the journal,
    // where a human reads it. So the human note is the default, and
    // `--json` asks for the structured form. This is the one
    // deliberate exception to the rule the other commands follow.
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&note).map_err(|_| Application(dg::error::Error::internal()))?
        );
    } else {
        let mut root = Section::new("backup");
        root.field(
            "state",
            Cell::styled(note.title.clone(), severity_style(&note.severity)),
        );
        root.field(
            "stale after",
            Cell::text(dg::render::human_duration(note.stale_after_secs)),
        );
        if let Some(failure) = &health.last_failure {
            root.field(
                "last failure",
                Cell::styled(
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
                    ),
                    Style::Red,
                ),
            );
        }
        root.field("action", Cell::text(note.action.clone()));

        let mut sections = vec![root];
        if !health.databases.is_empty() {
            let mut per_database = Section::new("per-database");
            let mut table = Table::new();
            table.column("database", Align::Left);
            table.column("state", Align::Left);
            table.column("newest set", Align::Left);
            table.column("age", Align::Right);
            for db in &health.databases {
                table.row(vec![
                    Cell::styled(db.name.clone(), Style::Cyan),
                    Cell::styled(db.state, state_style(db.state)),
                    Cell::text(
                        db.newest_set
                            .as_deref()
                            .map(dg::render::human_set_stamp)
                            .unwrap_or_else(|| "—".to_string()),
                    ),
                    Cell::text(
                        db.age_secs
                            .map(dg::render::human_duration)
                            .unwrap_or_else(|| "unknown".to_string()),
                    ),
                ]);
            }
            per_database.table(table);
            sections.push(per_database);
        }
        let mut out = std::io::stdout();
        dg::render::write_sections(&mut out, &terminal_theme(), &sections).map_err(Application)?;
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

pub async fn verify(backup_root: PathBuf, set: String, json: bool) -> Result<(), dg::ServerError> {
    let report = dg::backup::verify_set(&backup_root, &set).map_err(Application)?;
    let value = serde_json::to_value(&report).map_err(|_| Application(Error::internal()))?;
    if json || !std::io::stdout().is_terminal() {
        println!("{value}");
    } else {
        // The report is the point of the command, so it is drawn: which
        // databases were checked, how big they are, and what failed.
        let mut out = std::io::stdout();
        let theme = dg::render::Theme::for_stream(true);
        let mut table = Table::new();
        table.column("database", Align::Left);
        table.column("size", Align::Right);
        table.column("sha256", Align::Left);
        for database in report.databases.iter() {
            table.row(vec![
                Cell::styled(database.name.clone(), Style::Cyan),
                Cell::number(dg::render::human_bytes(database.size)),
                Cell::text(&database.sha256),
            ]);
        }
        let headline = if report.ok {
            format!("verified {}", report.set)
        } else {
            format!("FAILED {}", report.set)
        };
        let mut section = dg::render::Section::new("");
        section.field(
            "",
            Cell::styled(headline, if report.ok { Style::Green } else { Style::Red }),
        );
        if !report.failures.is_empty() {
            let mut failures = dg::render::Section::new("failures");
            for failure in &report.failures {
                failures.field("-", Cell::styled(failure.clone(), Style::Red));
            }
            dg::render::write_sections(&mut out, &theme, &[section, failures])
                .map_err(Application)?;
        } else {
            dg::render::write_sections(&mut out, &theme, &[section]).map_err(Application)?;
        }
        if !report.databases.is_empty() {
            writeln!(out).map_err(|_| Application(Error::internal()))?;
            table.write(&mut out, &theme).map_err(Application)?;
        }
    }
    if report.ok {
        return Ok(());
    }
    Err(Application(Error::new(
        400,
        "backup_verification_failed",
        "backup set failed verification; see JSON report",
    )))
}
