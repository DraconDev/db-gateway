//! One view layer: what a server answer means, drawn once for every
//! surface.
//!
//! The CLI, the interactive shell and the terminal UI all read their
//! facts from here. A view is a pure function of the JSON the gateway
//! returned: it never fetches, never draws, and never decides what
//! happens next.
//!
//! That is what keeps two surfaces from telling different stories about
//! the same gateway. The defect this module exists to prevent is
//! concrete: a batch the gateway refused was drawn as "not run" on one
//! surface and "rolled back" on another, because each surface read the
//! response on its own. Here the reading happens once, and every
//! surface draws the same reading.

use crate::render::{
    self, Align, Cell, Section, Style, Table, human_bytes, human_count, human_duration,
    human_epoch_ago, human_micros, human_percent, human_rate, human_set_stamp, wait_figures,
};
use serde_json::Value;

/// The service dashboard, grouped by what the reader is asking about.
///
/// The questions an operator has are "is it up", "is it busy", "is it
/// healthy" and "which of my databases is doing the work". The
/// dashboard answers those in that order, rather than listing the
/// server's properties.
pub fn service_dashboard(status: &serde_json::Value) -> Vec<Section> {
    let mut sections = Vec::new();

    // What the service is, and whether it is up.
    let mut service = Section::new("service");
    service.field(
        "version",
        Cell::text(status["version"].as_str().unwrap_or("?")),
    );
    service.field(
        "uptime",
        Cell::text(
            status["uptime_secs"]
                .as_u64()
                .map(human_duration)
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    );
    // A gateway older than `database_counts` answers without the field. It
    // must not be rendered as "0 live" -- the pane then says the gateway
    // holds no databases while the list beside it shows two, and an
    // operator reading that cannot tell an empty gateway from an
    // unanswerable question. `unknown` is what this codebase already says
    // for a figure that was never reported ("no waits recorded", "unknown"
    // backup freshness), so the same rule applies here.
    let counts = match (
        status["database_counts"]["live"].as_u64(),
        status["database_counts"]["tombstoned"].as_u64(),
    ) {
        (Some(live), Some(tombstoned)) => format!("{live} live, {tombstoned} tombstoned"),
        _ => "unknown".to_string(),
    };
    service.field("databases", Cell::text(counts));
    sections.push(service);

    // Where this gateway is, and where it keeps its files. The facts a
    // client cannot work out for itself: a gateway reached through a
    // tunnel is on a machine the client knows nothing about, and that
    // machine is the one holding the data.
    sections.push(deployment(status));

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
    sections.push(activity);

    // The recent shape of the traffic, as a chart in one line. Its own
    // section, so the chart starts at the left margin instead of being
    // pushed across by the key column.
    if let Some(values) = requests["recent_per_second"].as_array() {
        let counts: Vec<u64> = values.iter().filter_map(Value::as_u64).collect();
        if !counts.is_empty() {
            let mut recent = Section::new("recent activity");
            recent.field("req/s", Cell::Sparkline(counts));
            sections.push(recent);
        }
    }

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
        per_database.table(per_database_table(status));
        sections.push(per_database);
    }

    // Where each database actually is, and whether it is there. This is
    // the dashboard's last section because it is the slowest-changing
    // fact here and the one an operator goes to when the rest of the
    // picture is not making sense.
    if let Some(table) = locations(status) {
        let mut placement = Section::new("locations");
        placement.table(table);
        sections.push(placement);
    }

    sections
}

/// Where the gateway is running, and where it keeps its files.
///
/// The client cannot answer this for itself. A gateway is usually reached
/// through a tunnel, so from the client it is a loopback port and nothing
/// more -- the machine on the other end of the tunnel is the one holding
/// the data, and only the gateway knows which machine that is.
///
/// A gateway that does not report its placement (an older build) says so
/// once, rather than drawing four "unknown" lines that read like a
/// gateway which has lost its files.
pub fn deployment(status: &Value) -> Section {
    let mut section = Section::new("deployment");
    let Some(deployment) = status["deployment"].as_object() else {
        section.field("placement", Cell::text("not reported by this gateway"));
        return section;
    };
    section.field(
        "host",
        Cell::styled(
            deployment["host"].as_str().unwrap_or("unknown").to_string(),
            Style::Cyan,
        ),
    );
    section.field(
        "listen",
        Cell::text(
            deployment["listen"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
        ),
    );
    section.field(
        "data dir",
        Cell::styled(
            deployment["data_dir"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            Style::Cyan,
        ),
    );
    // The backup root is configured whether or not the directory is
    // readable, so this is the path, not a claim that anything is
    // backed up. Whether it is fresh is the backups tab's answer.
    let backup_root = match deployment["backup_root"].as_str() {
        Some(root) => root.to_string(),
        None => "none configured".to_string(),
    };
    section.field("backup root", Cell::styled(backup_root, Style::Cyan));
    // The room left on the filesystem the data lives on. The
    // observability contract names this as the disk-pressure signal: low
    // disk blocks safe writes, imports and staging.
    section.field(
        "disk free",
        match deployment.get("disk").and_then(Value::as_object) {
            Some(disk) => {
                let free = disk["free_bytes"].as_u64().unwrap_or(0);
                let total = disk["total_bytes"].as_u64().unwrap_or(0);
                let percent = disk["used_percent"].as_u64().unwrap_or(0);
                Cell::text(format!(
                    "{} of {} ({percent}% used)",
                    human_bytes(free),
                    human_bytes(total)
                ))
            }
            None => Cell::text("unknown"),
        },
    );
    // How much the database files themselves occupy.
    section.field(
        "databases",
        match deployment.get("databases_bytes").and_then(Value::as_u64) {
            Some(bytes) => Cell::text(human_bytes(bytes)),
            None => Cell::text("unknown"),
        },
    );
    // How long a deleted name is held before it is purged.
    section.field(
        "retention",
        match deployment.get("retention_secs").and_then(Value::as_i64) {
            Some(seconds) if seconds >= 0 => Cell::text(human_duration(seconds as u64)),
            _ => Cell::text("unknown"),
        },
    );
    section
}

/// Where each database lives on disk, and whether it is there.
///
/// `None` when the gateway does not report its placement. An empty table
/// would be a different claim -- a gateway with no databases -- and the
/// two must not look alike.
pub fn locations(status: &Value) -> Option<Table> {
    let databases = status["deployment"]["databases"].as_object()?;
    let mut table = Table::new();
    table.column("database", Align::Left);
    table.column("file", Align::Left);
    table.column("size", Align::Right);
    table.column("state", Align::Left);
    for (name, entry) in databases {
        let (state, style) = location_state(entry);
        table.row(vec![
            Cell::styled(name.clone(), Style::Cyan),
            Cell::text(entry["file"].as_str().unwrap_or("?").to_string()),
            Cell::text(
                entry["size_bytes"]
                    .as_u64()
                    .map(human_bytes)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            Cell::styled(state, style),
        ]);
    }
    Some(table)
}

/// The state of one database file: live, tombstoned, or missing.
///
/// A tombstone wins over a missing file: the name is still held and the
/// row is still on its way to being purged, which is what the operator
/// needs to know. A live database whose file is not there is the real
/// fault, and it is reported as one.
fn location_state(entry: &Value) -> (String, Style) {
    if entry["tombstoned"].as_bool() == Some(true) {
        return ("tombstoned".to_string(), Style::Yellow);
    }
    if entry["exists"].as_bool() != Some(true) {
        return ("missing".to_string(), Style::Red);
    }
    ("live".to_string(), Style::Green)
}

/// The per-database traffic table, over the databases that have done
/// something.
///
/// The names come from the service's own counters, so this is the
/// dashboard's answer to "which of my databases is doing the work".
pub fn per_database_table(status: &Value) -> Table {
    let names: Vec<String> = status["databases"]
        .as_object()
        .map(|usage| usage.keys().cloned().collect())
        .unwrap_or_default();
    traffic_table(&names, status)
}

/// The database list with each database's traffic.
///
/// The names come from the gateway's list and the traffic from the
/// service's own counters, so a database that has never been used is
/// on screen with zeroes rather than missing: "no traffic" and "not a
/// database" must not look the same.
///
/// The columns are the dashboard's own, so the terminal view's
/// database list and the CLI's dashboard are the same table.
pub fn databases(list: &Value, status: &Value) -> Table {
    let mut names: Vec<String> = list["databases"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|name| name.as_str().unwrap_or("").to_string())
        .collect();
    names.sort();
    traffic_table(&names, status)
}

/// One traffic table over a list of database names.
///
/// Every figure is shown. There used to be a narrower variant for the
/// terminal view's database *pane*, when the tiled layout gave each
/// answer a fraction of the screen; the view is tabbed now, so the
/// database list is a whole tab and has the width for all of them.
fn traffic_table(names: &[String], status: &Value) -> Table {
    let mut table = Table::new();
    table.column("database", Align::Left);
    table.column("rows read", Align::Right);
    table.column("rows written", Align::Right);
    table.column("waits", Align::Right);
    table.column("wait p95", Align::Right);
    table.column("wait max", Align::Right);
    table.column("last write", Align::Left);
    for name in names {
        let entry = &status["databases"][name.as_str()];
        let waits = entry["queue_waits"].as_u64().unwrap_or(0);
        // The p95 is this database's own, and the max is its worst
        // single wait. They are different facts, and the pairing is
        // built in one place so the two columns cannot be swapped.
        let figures_cells = wait_figures(entry, "-");
        let last_write = match entry["last_write"].as_u64() {
            Some(epoch) => Cell::text(human_epoch_ago(epoch)),
            None => Cell::text("never"),
        };
        table.row(vec![
            Cell::styled(name.clone(), Style::Cyan),
            Cell::number(entry["rows_read"].as_u64().unwrap_or(0)),
            Cell::number(entry["rows_written"].as_u64().unwrap_or(0)),
            Cell::number(waits),
            figures_cells.p95,
            figures_cells.max,
            last_write,
        ]);
    }
    table.footer("wait p95 is this database's own recent waits; wait max is its worst single wait");
    table
}

/// The backup freshness view: the root the gateway watches, the
/// published sets, and the per-database state.
pub fn backups(backups: &Value) -> Vec<Section> {
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
                .map(human_duration)
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    );
    let sets = backups["sets"].as_array().cloned().unwrap_or_default();
    root.field("sets", Cell::number(sets.len()));

    let mut sections = vec![root];

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
                    Cell::styled(state, backup_state_style(state)),
                    Cell::text(
                        fresh["newest_set"]
                            .as_str()
                            .map(human_set_stamp)
                            .unwrap_or_else(|| "—".to_string()),
                    ),
                    Cell::text(
                        fresh["age_secs"]
                            .as_u64()
                            .map(human_duration)
                            .unwrap_or_else(|| "unknown".to_string()),
                    ),
                ]);
            }
            freshness.table(table);
            sections.push(freshness);
        }
    }

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
                        .map(render::human_epoch)
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

    sections.extend(copy_sections(backups));
    sections
}

/// The sections that answer "are my copies any good, and where are they".
///
/// Four questions, each with its own section, because they are four
/// different claims and a section that merged them would hide which one
/// was missing:
///
/// - the last run: when a backup last happened and whether it worked,
/// - coverage: what the newest set actually carries,
/// - the replica: where the off-host copy is,
/// - the drill: whether that copy was ever restored and verified.
///
/// A fact the host does not report is named as unreported. A missing
/// section would be read as "there is nothing to say", which is a
/// different and much worse claim.
fn copy_sections(backups: &Value) -> Vec<Section> {
    let mut sections = Vec::new();

    // The last attempt, and the last failure when it is not the same
    // record: a failure that has been followed by a success is history,
    // and history is worth seeing, but it must not be drawn twice.
    if let Some(attempt) = backups["last_attempt"].as_object() {
        let mut section = Section::new("last run");
        let outcome = attempt["outcome"].as_str().unwrap_or("unknown");
        section.field(
            "outcome",
            Cell::styled(outcome.to_string(), attempt_outcome_style(outcome)),
        );
        section.field(
            "finished",
            Cell::text(
                attempt["completed_at"]
                    .as_u64()
                    .map(human_epoch_ago)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        );
        section.field(
            "set",
            Cell::text(format!(
                "{} ({} databases, step {})",
                attempt["set"]
                    .as_str()
                    .map(human_set_stamp)
                    .unwrap_or_else(|| "none published".to_string()),
                attempt["databases"].as_u64().unwrap_or(0),
                attempt["step"].as_str().unwrap_or("unknown"),
            )),
        );
        sections.push(section);
    }
    if let Some(failure) = backups["last_failure"].as_object()
        && backups["last_attempt"]["ts"] != failure["ts"]
    {
        let mut section = Section::new("last failure");
        section.field(
            "when",
            Cell::text(format!(
                "{} (step {})",
                failure["completed_at"]
                    .as_u64()
                    .map(human_epoch_ago)
                    .unwrap_or_else(|| "unknown".to_string()),
                failure["step"].as_str().unwrap_or("unknown"),
            )),
        );
        sections.push(section);
    }

    // What the newest set carries. The registry matters as much as the
    // data: without it the copy holds files that no key can open.
    match backups["coverage"].as_object() {
        Some(coverage) => {
            let mut section = Section::new("coverage");
            let live = coverage["live"].as_u64().unwrap_or(0);
            let carried = coverage["in_newest_set"].as_u64().unwrap_or(0);
            section.field(
                "newest set",
                Cell::text(format!("{carried} of {live} live databases")),
            );
            let missing: Vec<String> = coverage["missing"]
                .as_array()
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            section.field(
                "missing",
                Cell::styled(
                    if missing.is_empty() {
                        "none".to_string()
                    } else {
                        missing.join(", ")
                    },
                    if missing.is_empty() {
                        Style::Green
                    } else {
                        Style::Yellow
                    },
                ),
            );
            let registry = coverage.get("registry").and_then(Value::as_bool) == Some(true);
            section.field(
                "registry",
                Cell::styled(
                    if registry { "yes" } else { "no" }.to_string(),
                    if registry {
                        Style::Green
                    } else {
                        Style::Yellow
                    },
                ),
            );
            sections.push(section);
        }
        None => sections.push(unreported("coverage", "the gateway reports no coverage")),
    }

    // Where the off-host copy is. The gateway does not know this by
    // itself; the host publishes it, and the answer says so when there
    // is none.
    match backups["replica"].as_object() {
        Some(replica) => {
            let mut section = Section::new("replica");
            // The bucket when the copy is an object store, and the target
            // itself when it is not: "where the copy is" is one fact, and
            // the object-store case names it by its bucket.
            let where_copy_lives = replica
                .get("bucket")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    replica
                        .get("target")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "unknown".to_string());
            section.field("bucket", Cell::styled(where_copy_lives, Style::Cyan));
            section.field(
                "endpoint",
                Cell::text(format!(
                    "{} ({})",
                    replica
                        .get("endpoint")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    replica
                        .get("region")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                )),
            );
            section.field(
                "retention",
                Cell::text(format!(
                    "{} (snapshot {})",
                    replica
                        .get("retention")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    replica
                        .get("snapshot_interval")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                )),
            );
            let databases: Vec<String> = replica
                .get("databases")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            // The registry is a database too, and the one without which the
            // copies hold files that no key can open -- so it is named
            // beside them rather than in a line of its own.
            let registry = replica.get("registry").and_then(Value::as_bool) == Some(true);
            section.field(
                "databases",
                Cell::text(format!(
                    "{}{}",
                    if databases.is_empty() {
                        "none".to_string()
                    } else {
                        databases.join(", ")
                    },
                    if registry { " (+registry)" } else { "" },
                )),
            );
            // Whether the thing that does the copying is running. A
            // configured replica that is not replicating is the failure
            // that looks most like success from a distance.
            let sidecar = replica
                .get("sidecar")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            section.field(
                "sidecar",
                Cell::styled(
                    format!(
                        "{sidecar} (published {})",
                        replica
                            .get("published_at")
                            .and_then(Value::as_u64)
                            .map(human_epoch_ago)
                            .unwrap_or_else(|| "unknown".to_string())
                    ),
                    sidecar_style(sidecar),
                ),
            );
            sections.push(section);
        }
        None => sections.push(unreported(
            "replica",
            "this host publishes no off-host copy status",
        )),
    }

    // Whether the off-host copy has ever been restored and verified. A
    // backup that was never restored is an assumption.
    match backups["drill"].as_object() {
        Some(drill) => {
            let mut section = Section::new("restore drill");
            let outcome = drill["outcome"].as_str().unwrap_or("unknown");
            section.field(
                "outcome",
                Cell::styled(outcome.to_string(), attempt_outcome_style(outcome)),
            );
            section.field(
                "ran",
                Cell::text(
                    drill["drilled_at"]
                        .as_u64()
                        .map(human_epoch_ago)
                        .unwrap_or_else(|| "unknown".to_string()),
                ),
            );
            section.field(
                "databases",
                Cell::text(format!(
                    "{} databases{}",
                    drill["databases"].as_u64().unwrap_or(0),
                    match drill["detail"].as_str() {
                        Some(detail) => format!(", {detail}"),
                        None => String::new(),
                    }
                )),
            );
            sections.push(section);
        }
        None => sections.push(unreported(
            "restore drill",
            "this host publishes no restore drill result",
        )),
    }

    sections
}

/// A section that says a fact is not reported.
///
/// The point is that a missing section reads as "there is nothing to
/// say", which is a different claim from "nobody said". A gateway that
/// does not report its placement, its replica or its drill is telling
/// the operator exactly that, in words.
fn unreported(title: &str, why: &str) -> Section {
    let mut section = Section::new(title);
    section.field("reported", Cell::text(why));
    section
}

/// The style for the replication sidecar's state.
///
/// The word is always spelled out, so the colour is a second rendering of
/// the same fact rather than the only rendering of it.
pub fn sidecar_style(state: &str) -> Style {
    match state {
        "active" => Style::Green,
        "inactive" | "failed" => Style::Red,
        _ => Style::Yellow,
    }
}

/// The style for a run's outcome.
///
/// The word is always spelled out, so the colour is a second rendering of
/// the same fact rather than the only rendering of it.
pub fn attempt_outcome_style(outcome: &str) -> Style {
    match outcome {
        "ok" => Style::Green,
        "failed" | "error" => Style::Red,
        _ => Style::Yellow,
    }
}

/// The style for a backup freshness state.
///
/// The state word itself is always spelled out, so the colour is a
/// second rendering of the same information rather than the only
/// rendering of it.
pub fn backup_state_style(state: &str) -> Style {
    match state {
        "fresh" => Style::Green,
        "stale" => Style::Yellow,
        _ => Style::Red,
    }
}

/// The credential list: every outstanding key with its role and the
/// databases it may reach.
///
/// The owner key is the one that can do everything, so it is worth
/// seeing at a glance in a list of credentials.
pub fn keys(keys: &Value) -> Table {
    let mut table = Table::new();
    table.column("name", Align::Left);
    table.column("role", Align::Left);
    table.column("databases", Align::Left);
    for key in keys["keys"].as_array().cloned().unwrap_or_default().iter() {
        let databases = key["databases"]
            .as_array()
            .map(|dbs| {
                dbs.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let role = key["role"].as_str().unwrap_or("");
        table.row(vec![
            Cell::styled(key["name"].as_str().unwrap_or(""), Style::Cyan),
            Cell::styled(
                role,
                if role == "owner" {
                    Style::Yellow
                } else {
                    Style::Plain
                },
            ),
            Cell::text(if databases.is_empty() && role == "owner" {
                "all".to_string()
            } else {
                databases
            }),
        ]);
    }
    table
}

/// The database name list.
pub fn database_names(databases: &Value) -> Table {
    let names: Vec<String> = databases["databases"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|name| name.as_str().unwrap_or("").to_string())
        .collect();
    let mut table = Table::new();
    table.column("database", Align::Left);
    for name in &names {
        table.row(vec![Cell::styled(name.clone(), Style::Cyan)]);
    }
    table.footer(if names.len() == 1 {
        "1 database".to_string()
    } else {
        format!("{} databases", names.len())
    });
    table
}

/// What one database has been doing, as the `activity` section.
///
/// The p95 and the max are different facts and are labelled as such:
/// the p95 says how bad this database's queue usually is, the max says
/// how bad it ever got. The label and the value travel together from
/// the shared helper, so this view and the dashboard cannot disagree
/// about which is which.
pub fn activity_section(entry: &Value) -> Section {
    let mut activity = Section::new("activity");
    activity.field(
        "rows read",
        Cell::number(entry.get("rows_read").and_then(Value::as_u64).unwrap_or(0)),
    );
    activity.field(
        "rows written",
        Cell::number(
            entry
                .get("rows_written")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
    );
    let waits = entry
        .get("queue_waits")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    activity.field("queue waits", Cell::number(waits));
    for (label, cell) in render::wait_figure_fields(entry, "no waits recorded") {
        activity.field(label, cell);
    }
    activity.field(
        "last write",
        match entry.get("last_write").and_then(Value::as_u64) {
            Some(epoch) => Cell::text(last_write_ago(epoch)),
            None => Cell::text("never"),
        },
    );
    activity
}

/// How long ago a write happened, as a person reads it.
pub fn last_write_ago(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch);
    format!("{} ago", human_duration(now.saturating_sub(epoch)))
}

/// What a batch of statements did, read once so every surface agrees.
///
/// The gateway answers a batch in one of two ways: a list of results,
/// one per statement, or an error. The error may name the statement
/// that broke it, or may not — and what an unnamed error MEANS depends
/// on how many statements the script held, which only the client
/// knows: the gateway attributes an error to a statement only when the
/// batch holds more than one, because a lone statement is unambiguous
/// on its own.
///
/// So a script of one statement that fails carries no index, and that
/// absence says nothing about whether it ran: its row carries the
/// reason. A script of several that fails without an index was refused
/// as a whole, before anything ran, so no row claims a rollback that
/// did not happen.
#[derive(Clone, Debug)]
pub struct BatchOutcome {
    /// One row per statement, in the order they were sent.
    pub table: Table,
    /// The gateway's error code, when the request failed.
    pub code: Option<String>,
    /// The gateway's error message, when the request failed.
    pub message: Option<String>,
    /// The statement the gateway blamed, when it named one.
    pub blamed: Option<usize>,
    /// True when the request was refused as a whole: nothing ran, and
    /// nothing was rolled back either.
    pub refused: bool,
}

impl BatchOutcome {
    /// True when the batch ran and every statement has a result.
    pub fn ran(&self) -> bool {
        self.code.is_none()
    }

    /// The failure as one line, for a status bar or an error message.
    pub fn reason(&self) -> Option<String> {
        match (&self.code, &self.message) {
            (Some(code), Some(message)) => Some(format!("{code}: {message}")),
            (Some(code), None) => Some(code.clone()),
            _ => None,
        }
    }
}

/// Read a batch response once, for every surface that draws it.
pub fn batch_outcomes(statements: &[String], result: &Value) -> BatchOutcome {
    if let Some(error) = result.get("error") {
        let message = error["message"].as_str().unwrap_or("failed").to_string();
        let code = error["code"].as_str().unwrap_or("error").to_string();
        let blamed = error["statement"].as_u64().map(|index| index as usize);
        let mut table = Table::new();
        table.column("#", Align::Right);
        table.column("statement", Align::Left);
        table.column("result", Align::Left);
        for (index, statement) in statements.iter().enumerate() {
            let outcome = match blamed {
                Some(blamed) if blamed == index => {
                    Cell::styled(format!("{code}: {message}"), Style::Red)
                }
                // A script of one statement: the gateway names no
                // statement for it, so the row shows why it failed. The
                // statement did run — "not run" would be a lie, and the
                // reason would be missing from the view.
                _ if statements.len() == 1 => {
                    Cell::styled(format!("{code}: {message}"), Style::Red)
                }
                // A script of several, refused without naming a
                // statement: the whole request was rejected, so nothing
                // ran and nothing was rolled back.
                None => Cell::styled("not run", Style::Dim),
                // A statement broke the batch: the rest were rolled
                // back with it.
                _ => Cell::styled("rolled back", Style::Dim),
            };
            table.row(vec![
                Cell::number(index + 1),
                Cell::text(statement),
                outcome,
            ]);
        }
        return BatchOutcome {
            table,
            code: Some(code),
            message: Some(message),
            blamed,
            // A request that names no statement, in a script of several,
            // was refused as a whole, before anything ran.
            refused: blamed.is_none() && statements.len() > 1,
        };
    }
    let mut table = Table::new();
    table.column("#", Align::Right);
    table.column("statement", Align::Left);
    table.column("result", Align::Right);
    let results = result["results"].as_array().cloned().unwrap_or_default();
    for (index, statement) in statements.iter().enumerate() {
        let outcome = match results.get(index) {
            Some(one) => match one["error"].as_str() {
                Some(message) => Cell::styled(message.to_string(), Style::Red),
                None => match one["rows_affected"].as_u64() {
                    Some(rows) => Cell::text(format!("{rows} rows")),
                    None => {
                        let rows = one["rows"].as_array().map(|rows| rows.len()).unwrap_or(0);
                        Cell::text(format!("{rows} rows"))
                    }
                },
            },
            None => Cell::text("-"),
        };
        table.row(vec![
            Cell::number(index + 1),
            Cell::text(statement),
            outcome,
        ]);
    }
    BatchOutcome {
        table,
        code: None,
        message: None,
        blamed: None,
        refused: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table as plain text, so a test can read what an operator reads.
    fn render_table(table: &Table) -> String {
        let mut out = Vec::new();
        table
            .write(&mut out, &render::Theme::fixed(false, true))
            .expect("a table renders");
        String::from_utf8(out).expect("a table is text")
    }

    #[test]
    fn the_inspect_view_labels_the_p95_and_the_max_under_their_own_names() {
        // The defect this protects against: the wait-latency field once
        // held the database's MAXIMUM wait under a "wait p95" label.
        // The two values here are far apart, so a swapped pair is
        // visible; a row-wide assertion could not tell.
        let entry = serde_json::json!({
            "rows_read": 1,
            "rows_written": 2,
            "queue_waits": 21,
            "queue_wait_p95_us": 1_000,
            "queue_wait_max_us": 900_000,
        });
        let section = activity_section(&entry);
        let mut out = Vec::new();
        section
            .write(&mut out, &render::Theme::fixed(false, true))
            .unwrap();
        let text = String::from_utf8(out).unwrap();

        let p95_line = text
            .lines()
            .find(|line| line.trim_start().starts_with("wait p95"))
            .unwrap_or_else(|| panic!("no wait p95 field: {text}"));
        let max_line = text
            .lines()
            .find(|line| line.trim_start().starts_with("wait max"))
            .unwrap_or_else(|| panic!("no wait max field: {text}"));

        assert!(
            p95_line.contains("1.0 ms"),
            "the wait p95 field does not hold the p95: {p95_line}"
        );
        assert!(
            max_line.contains("900 ms"),
            "the wait max field does not hold the max: {max_line}"
        );
    }

    /// The cells of a rendered table row, split on the box drawing.
    fn row_cells(text: &str, row_needle: &str) -> Vec<String> {
        text.lines()
            .find(|line| line.contains(row_needle))
            .expect("the row is not in the output")
            .split('│')
            .map(|cell| cell.trim().to_string())
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    /// The header cells of the table the row belongs to.
    fn header_cells(text: &str) -> Vec<String> {
        text.lines()
            .find(|line| line.contains("wait p95") && line.contains("wait max"))
            .expect("the table header is not in the output")
            .split('│')
            .map(|cell| cell.trim().to_string())
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    #[test]
    fn a_gateway_that_does_not_report_counts_is_not_drawn_as_holding_none() {
        // A gateway older than `database_counts` answers without the field.
        // Rendering the absent figure as 0 made the service pane say "0 live"
        // while the database list beside it showed two rows -- an operator
        // cannot tell an empty gateway from one that never answered the
        // question. The view already refuses to invent a measurement
        // elsewhere ("no waits recorded"), so it must not invent this one.
        let status = serde_json::json!({
            "version": "0.1.0",
            "uptime_secs": 60,
            "requests": {
                "total": 1,
                "capacity_exceeded": 0,
                "query_deadline_exceeded": 0,
                "internal_error": 0,
                "recent_per_second": [],
                "recent_span_secs": 0,
            },
            "databases": {"demo": {"rows_read": 1, "rows_written": 0}},
        });
        let sections = service_dashboard(&status);
        let mut out = Vec::new();
        render::write_sections(&mut out, &render::Theme::fixed(false, true), &sections).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("0 live"),
            "an unreported count must not be drawn as zero live databases:\n{text}"
        );
        assert!(
            text.contains("unknown"),
            "an unreported count must be drawn as unknown:\n{text}"
        );
    }

    #[test]
    fn a_present_count_still_reads_as_a_number() {
        // The other side of the rule: when the gateway does report the
        // counts, the pane shows them rather than "unknown".
        let status = serde_json::json!({
            "version": "0.2.0",
            "uptime_secs": 60,
            "database_counts": {"live": 2, "tombstoned": 1},
            "requests": {
                "total": 1,
                "capacity_exceeded": 0,
                "query_deadline_exceeded": 0,
                "internal_error": 0,
                "recent_per_second": [],
                "recent_span_secs": 0,
            },
            "databases": {},
        });
        let sections = service_dashboard(&status);
        let mut out = Vec::new();
        render::write_sections(&mut out, &render::Theme::fixed(false, true), &sections).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("2 live, 1 tombstoned"),
            "a reported count must be shown as reported:\n{text}"
        );
    }

    #[test]
    fn the_per_database_table_puts_the_p95_and_the_max_under_their_own_headers() {
        // The defect this protects against: the wait-latency column
        // once held each database's MAXIMUM wait under a "wait p95"
        // header, so on a contended database it overstated the
        // percentile and disagreed with the genuine p95 in the
        // admission block of the same screen.
        //
        // The two values here are deliberately far apart, so the test
        // can tell which cell holds which. A row-wide assertion cannot:
        // when a database has one wait, its p95 and its max are the
        // same number, and a swapped pair looks correct.
        let status = serde_json::json!({
            "version": "0.2.0",
            "uptime_secs": 60,
            "database_counts": {"live": 1, "tombstoned": 0},
            "requests": {
                "total": 10,
                "capacity_exceeded": 0,
                "query_deadline_exceeded": 0,
                "internal_error": 0,
                "recent_per_second": [1, 2, 3],
                "recent_span_secs": 3,
            },
            "databases": {
                "demo": {
                    "rows_read": 1,
                    "rows_written": 2,
                    "queue_waits": 21,
                    // The p95 is small; the max is far larger.
                    "queue_wait_p95_us": 1_000,
                    "queue_wait_max_us": 900_000,
                    "last_write": null,
                }
            },
        });
        let sections = service_dashboard(&status);
        let mut out = Vec::new();
        render::write_sections(&mut out, &render::Theme::fixed(false, true), &sections).unwrap();
        let text = String::from_utf8(out).unwrap();

        let header = header_cells(&text);
        let p95_column = header
            .iter()
            .position(|cell| cell == "wait p95")
            .expect("no wait p95 column");
        let max_column = header
            .iter()
            .position(|cell| cell == "wait max")
            .expect("no wait max column");
        assert_ne!(p95_column, max_column, "the two columns are the same");

        let row = row_cells(&text, "demo");
        // Each cell holds the value its own header names, and not the
        // other's: 1 ms is the p95, 900 ms is the max.
        assert_eq!(
            row[p95_column], "1.0 ms",
            "the wait p95 column does not hold the p95: {text}"
        );
        assert_eq!(
            row[max_column], "900 ms",
            "the wait max column does not hold the max: {text}"
        );
        // And the two are not the same cell, so neither can be the
        // other by accident.
        assert_ne!(row[p95_column], row[max_column], "{text}");
    }

    #[test]
    fn a_lone_statement_that_fails_carries_the_reason_not_a_claim_about_running() {
        // The gateway names no statement for a batch of one, so the
        // absence of an index says nothing about whether it ran. A row
        // that said "not run" would be a claim the gateway never made.
        let outcome = batch_outcomes(
            &["select 1".to_string()],
            &serde_json::json!({"error": {"code": "sql_error", "message": "no such table: t"}}),
        );
        let text = render_table(&outcome.table);
        assert!(
            text.contains("sql_error: no such table: t"),
            "the row does not carry the reason: {text}"
        );
        assert!(
            !text.contains("not run"),
            "the row claims the statement did not run: {text}"
        );
        assert!(!outcome.ran(), "a failed batch was read as one that ran");
        assert!(
            !outcome.refused,
            "a lone statement's failure was read as a whole-request refusal"
        );
    }

    #[test]
    fn a_refused_batch_of_several_says_not_run_and_claims_no_rollback() {
        // A request refused before anything ran rolled nothing back, so
        // a row that said "rolled back" would claim a rollback that did
        // not happen.
        let outcome = batch_outcomes(
            &["select 1".to_string(), "select 2".to_string()],
            &serde_json::json!({"error": {"code": "too_many_statements", "message": "too long"}}),
        );
        let text = render_table(&outcome.table);
        assert_eq!(
            text.matches("not run").count(),
            2,
            "every row of a refused batch says not run: {text}"
        );
        assert!(
            !text.contains("rolled back"),
            "a refused batch claims a rollback: {text}"
        );
        assert!(outcome.refused, "a refused batch was not read as refused");
        assert_eq!(
            outcome.reason().as_deref(),
            Some("too_many_statements: too long"),
            "the refusal does not carry the gateway's reason"
        );
    }

    #[test]
    fn a_statement_that_broke_the_batch_rolls_the_rest_back() {
        let outcome = batch_outcomes(
            &["select 1".to_string(), "select 2".to_string()],
            &serde_json::json!({"error": {"code": "sql_error", "message": "boom", "statement": 1}}),
        );
        let text = render_table(&outcome.table);
        assert!(
            text.contains("rolled back"),
            "the statements after the broken one are not marked rolled back: {text}"
        );
        assert!(
            text.contains("sql_error: boom"),
            "the broken statement does not carry the reason: {text}"
        );
        assert!(
            !outcome.refused,
            "a batch that broke on a statement was read as a whole-request refusal"
        );
    }

    #[test]
    fn a_batch_that_ran_lists_one_row_per_statement() {
        let outcome = batch_outcomes(
            &["select 1".to_string(), "select 2".to_string()],
            &serde_json::json!({"results": [{"rows_affected": 3}, {"rows": [1, 2]}]}),
        );
        let text = render_table(&outcome.table);
        assert!(
            outcome.ran(),
            "a batch with results was not read as one that ran"
        );
        assert!(
            text.contains("3 rows"),
            "the first result is missing: {text}"
        );
        assert!(
            text.contains("2 rows"),
            "the second result is missing: {text}"
        );
    }

    /// A section as plain text, so a test can read what an operator reads.
    fn render_section(section: &Section) -> String {
        let mut out = Vec::new();
        section
            .write(&mut out, &render::Theme::fixed(false, true))
            .expect("a section renders");
        String::from_utf8(out).expect("a section is text")
    }

    /// A gateway that reports where it is and where its databases are.
    fn placed() -> serde_json::Value {
        serde_json::json!({
            "version": "0.2.0",
            "deployment": {
                "host": "micro2",
                "listen": "127.0.0.1:7432",
                "data_dir": "/var/lib/dg",
                "backup_root": "/var/lib/dg-backups",
                "databases": {
                    "alpha": {
                        "file": "/var/lib/dg/alpha.sqlite",
                        "exists": true,
                        "size_bytes": 40960,
                        "tombstoned": false
                    },
                    "beta": {
                        "file": "/var/lib/dg/beta.sqlite",
                        "exists": false,
                        "size_bytes": 0,
                        "tombstoned": false
                    },
                    "gone": {
                        "file": "/var/lib/dg/gone.sqlite",
                        "exists": true,
                        "size_bytes": 8192,
                        "tombstoned": true
                    }
                }
            }
        })
    }

    #[test]
    fn the_placement_says_where_the_gateway_and_its_files_are() {
        // The client cannot work this out for itself: a gateway reached
        // through a tunnel is a loopback port from the outside, and the
        // machine on the other end is the one holding the data.
        let text = render_section(&deployment(&placed()));
        for fact in [
            "micro2",
            "127.0.0.1:7432",
            "/var/lib/dg",
            "/var/lib/dg-backups",
        ] {
            assert!(
                text.contains(fact),
                "the placement does not say {fact:?}: {text}"
            );
        }
    }

    #[test]
    fn a_gateway_that_reports_no_placement_says_so_once() {
        // An older gateway has no placement to report. Drawing four
        // "unknown" lines would read like a gateway that has lost its
        // files; saying so once says what is actually true.
        let text = render_section(&deployment(&serde_json::json!({"version": "0.1.0"})));
        assert!(
            text.contains("not reported by this gateway"),
            "the placement does not say the gateway does not report it: {text}"
        );
        assert!(
            !text.contains("unknown"),
            "an unreported placement is drawn as unknown facts: {text}"
        );
    }

    #[test]
    fn each_database_is_placed_with_its_file_and_whether_it_is_there() {
        let table = locations(&placed()).expect("the gateway places its databases");
        let text = render_table(&table);
        for fact in [
            "/var/lib/dg/alpha.sqlite",
            "/var/lib/dg/beta.sqlite",
            "/var/lib/dg/gone.sqlite",
            "live",
            "missing",
            "tombstoned",
        ] {
            assert!(
                text.contains(fact),
                "the locations do not show {fact:?}: {text}"
            );
        }
        // The state belongs to the database it is about: a live file and a
        // tombstone are different things, and a state on another row would
        // be a different claim.
        let rows: Vec<&str> = text
            .lines()
            .filter(|line| line.contains(".sqlite"))
            .collect();
        assert_eq!(
            rows.len(),
            3,
            "the table does not hold one row per database"
        );
        assert!(
            rows[0].contains("alpha") && rows[0].contains("live"),
            "alpha is not live on its own row: {}",
            rows[0]
        );
        assert!(
            rows[1].contains("beta") && rows[1].contains("missing"),
            "beta is not missing on its own row: {}",
            rows[1]
        );
        assert!(
            rows[2].contains("gone") && rows[2].contains("tombstoned"),
            "the tombstone is not on its own row: {}",
            rows[2]
        );
    }

    /// The backups payload as a gateway that reports everything.
    fn copied() -> serde_json::Value {
        serde_json::json!({
            "backup_root": "/var/lib/dg-backups",
            "stale_after_secs": 300,
            "sets": [],
            "per_database": {},
            "last_attempt": {
                "ts": "20261024T090000Z",
                "completed_at": 1_764_000_000u64,
                "outcome": "ok",
                "set": "20261024T090000Z",
                "databases": 2,
                "step": "publish"
            },
            "coverage": {"live": 2, "in_newest_set": 1, "missing": ["other"], "registry": false},
            "replica": {
                "target": "s3://dg-sandbox",
                "bucket": "dg-sandbox",
                "endpoint": "s3.example.invalid",
                "region": "gra",
                "retention": "168h",
                "snapshot_interval": "24h",
                "databases": ["demo"],
                "registry": true,
                "sidecar": "active",
                "published_at": 1_764_000_060u64
            },
            "drill": {
                "drilled_at": 1_760_000_000u64,
                "outcome": "failed",
                "databases": 2,
                "detail": "not verified: other"
            }
        })
    }

    /// Every section of the copies view, as one text.
    fn copies_text(backups: &serde_json::Value) -> String {
        let mut text = String::new();
        for section in copy_sections(backups) {
            text.push_str(&render_section(&section));
            text.push('\n');
        }
        text
    }

    #[test]
    fn the_copies_view_says_where_the_off_host_copy_is_and_whether_it_restored() {
        // The four questions, each with its own section, because they are
        // four different claims.
        let text = copies_text(&copied());
        for fact in [
            "dg-sandbox",
            "s3.example.invalid",
            "gra",
            "168h",
            "publish",
            "1 of 2 live databases",
            "other",
            "not verified: other",
        ] {
            assert!(
                text.contains(fact),
                "the copies view does not say {fact:?}: {text}"
            );
        }
        // The state of the sidecar is spelled out: a configured replica
        // that is not replicating is the failure that looks most like
        // success from a distance.
        assert!(
            text.contains("active"),
            "the sidecar state is missing: {text}"
        );
        // The registry is not in the newest set, and that is said. The
        // fields are an aligned list, so the check is on the line that
        // names the registry rather than on a phrase.
        let registry_line = text
            .lines()
            .find(|line| line.contains("registry"))
            .unwrap_or_else(|| panic!("the registry is not mentioned at all: {text}"));
        assert!(
            registry_line.contains("no"),
            "a set without the registry is not called out: {registry_line}"
        );
    }

    #[test]
    fn a_host_that_publishes_nothing_says_so_rather_than_drawing_nothing() {
        // A missing section reads as "there is nothing to say", which is a
        // different and much worse claim than "nobody said".
        let text = copies_text(&serde_json::json!({"backup_root": "/var/lib/dg-backups"}));
        assert!(
            text.contains("this host publishes no off-host copy status"),
            "the replica is not called unreported: {text}"
        );
        assert!(
            text.contains("this host publishes no restore drill result"),
            "the drill is not called unreported: {text}"
        );
        assert!(
            text.contains("the gateway reports no coverage"),
            "the coverage is not called unreported: {text}"
        );
    }

    #[test]
    fn a_gateway_that_places_nothing_yields_no_table_rather_than_an_empty_one() {
        // An empty table is a different claim -- a gateway with no
        // databases -- and the two must not look alike.
        assert!(locations(&serde_json::json!({"version": "0.1.0"})).is_none());
    }
}
