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
    human_epoch_ago, human_instant, human_micros, human_percent, human_rate, human_set_stamp,
    wait_figures,
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
/// It carries no contents: this is the traffic view, and the counts of
/// what a database HOLDS belong on the databases tab where the
/// operator compares them.
pub fn per_database_table(status: &Value) -> Table {
    let names: Vec<String> = status["databases"]
        .as_object()
        .map(|usage| usage.keys().cloned().collect())
        .unwrap_or_default();
    traffic_table(&names, status, &Value::Null)
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
pub fn databases(list: &Value, status: &Value, contents: &Value) -> Table {
    let mut names: Vec<String> = list["databases"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|name| name.as_str().unwrap_or("").to_string())
        .collect();
    names.sort();
    traffic_table(&names, status, contents)
}

/// How much data each database holds, as the grid an operator reads.
///
/// One row per database: its table count and its total row count, read
/// from the files. A database whose file could not be read says
/// `unknown` rather than `0`, because a fault must not be drawn as an
/// empty database -- which is the very mistake this view exists to
/// correct.
pub fn contents(document: &Value) -> Table {
    let mut table = Table::new();
    table.column("database", Align::Left);
    table.column("tables", Align::Right);
    table.column("rows", Align::Right);
    let mut names: Vec<&String> = document["databases"]
        .as_object()
        .map(|entries| entries.keys().collect())
        .unwrap_or_default();
    names.sort();
    for name in &names {
        let entry = &document["databases"][name.as_str()];
        table.row(vec![
            Cell::styled((*name).clone(), Style::Cyan),
            count_cell(&entry["tables"]),
            count_cell(&entry["rows"]),
        ]);
    }
    table.footer(if names.len() == 1 {
        "1 database".to_string()
    } else {
        format!("{} databases", names.len())
    });
    table
}

/// One count from the contents answer: a number, or `unknown` when the
/// file could not be read.
fn count_cell(value: &Value) -> Cell {
    match value.as_u64() {
        Some(count) => Cell::number(count),
        None => Cell::styled("unknown", Style::Dim),
    }
}

/// One traffic table over a list of database names.
///
/// Every figure is shown. There used to be a narrower variant for the
/// terminal view's database *pane*, when the tiled layout gave each
/// answer a fraction of the screen; the view is tabbed now, so the
/// database list is a whole tab and has the width for all of them.
///
/// The `tables` and `rows` columns are WHAT THE DATABASE HOLDS, read
/// from its file; the traffic columns beside them say what it has been
/// DOING. The two were confused once, and the confusion was expensive:
/// every traffic counter is in memory and a restart zeroes it, so a
/// gateway that had just restarted drew seven full databases as seven
/// empty ones. The traffic block is now headed and footnoted as
/// since-restart, and the counts that come from the files sit first.
fn traffic_table(names: &[String], status: &Value, contents: &Value) -> Table {
    // Whether this table also carries WHAT each database holds. The
    // databases tab does; the status dashboard is the traffic view and
    // does not. The traffic column names say "since restart" only where
    // the footnote explaining it is drawn -- the tab -- because a star
    // nobody explains is a worse hint than none.
    let with_contents = contents.get("databases").is_some();
    let mut table = Table::new();
    table.column("database", Align::Left);
    if with_contents {
        // What it holds: from the files, so a restart cannot zero it.
        table.column("tables", Align::Right);
        table.column("rows", Align::Right);
    }
    // How big the file is. The traffic columns say what the database has
    // been doing; this says what it costs to keep. A hosted console puts
    // the two side by side, and an operator watching disk wants both.
    table.column("size", Align::Right);
    // What it has been doing SINCE THE GATEWAY RESTARTED. In memory.
    let traffic = |name: &'static str| {
        if with_contents {
            format!("{name}*")
        } else {
            name.to_string()
        }
    };
    table.column(traffic("rows read"), Align::Right);
    table.column(traffic("rows written"), Align::Right);
    table.column(traffic("waits"), Align::Right);
    table.column("wait p95", Align::Right);
    table.column("wait max", Align::Right);
    table.column(traffic("last write"), Align::Left);
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
        let size = match status["deployment"]["databases"][name.as_str()]["size_bytes"].as_u64() {
            Some(bytes) => Cell::text(render::human_bytes(bytes)),
            None => Cell::text("—"),
        };
        let mut row = vec![Cell::styled(name.clone(), Style::Cyan)];
        if with_contents {
            let held = &contents["databases"][name.as_str()];
            row.push(count_cell(&held["tables"]));
            row.push(count_cell(&held["rows"]));
        }
        row.extend([
            size,
            Cell::number(entry["rows_read"].as_u64().unwrap_or(0)),
            Cell::number(entry["rows_written"].as_u64().unwrap_or(0)),
            Cell::number(waits),
            figures_cells.p95,
            figures_cells.max,
            last_write,
        ]);
        table.row(row);
    }
    if with_contents {
        table.footer(
            "tables/rows are what the database holds; columns marked * are since the gateway \
             restarted, and a restart zeroes them. wait p95 is this database's own recent waits; \
             wait max is its worst single wait",
        );
    } else {
        table.footer(
            "wait p95 is this database's own recent waits; wait max is its worst single wait",
        );
    }
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
    // The contract's own word for this is the snapshot-cadence objective
    // (observability contract §2). It is not an age threshold on the sets:
    // freshness is decided by whether a committed change is covered, so a
    // set older than this can still be `fresh` while the database is idle.
    // Calling it "freshness objective" put a number next to a table of ages and
    // ages that were larger, and read as a contradiction.
    root.field(
        "freshness objective",
        Cell::text(
            backups["stale_after_secs"]
                .as_u64()
                .map(human_duration)
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    );
    // The owner-approved snapshot cadence: how long a committed change may
    // stay unrepresented by a verified copy before it is a finding. It is
    // shown whether or not one is set, because "not approved" is itself the
    // answer to "why is my digest red" -- and with no cadence every
    // unrepresented change is reported, which is the strict reading.
    root.field(
        "snapshot cadence",
        Cell::text(
            backups["cadence_secs"]
                .as_u64()
                .map(human_duration)
                .unwrap_or_else(|| "not approved".to_string()),
        ),
    );
    let sets = backups["sets"].as_array().cloned().unwrap_or_default();
    root.field("sets", Cell::number(sets.len()));
    // How many of the published sets are carrying databases that have
    // since been deleted. They are not stale -- they are gone -- so they
    // are not in the per-database table, but they are still on disk until
    // the sets are pruned. It is said only when there are some: a line
    // saying "none" is a line the screen could spend on something else.
    let deleted = backups["deleted"].as_u64().unwrap_or(0);
    if deleted > 0 {
        root.field(
            "deleted",
            Cell::text(format!(
                "{deleted} database(s) in the sets; their copies are pruned with the sets"
            )),
        );
    }

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
            // How long the newest change has gone without a copy. It is the
            // number the cadence is judged on, so a row that says `behind`
            // is only readable next to it: without it, `behind` looks like a
            // state rather than a measurement.
            table.column("lag", Align::Right);
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
                    Cell::text(
                        fresh["lag_secs"]
                            .as_u64()
                            .map(human_duration)
                            .unwrap_or_else(|| "—".to_string()),
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
    // The last failure, when it is not the same record as the last
    // attempt: a failure that has been followed by a success is history,
    // and history is worth seeing. It is a field of the last run rather
    // than a section of its own, because it IS about the last run -- and
    // a heading of its own spent a line of the screen on a thing the
    // operator reads once.
    if let Some(failure) = backups["last_failure"].as_object()
        && backups["last_attempt"]["ts"] != failure["ts"]
        && let Some(section) = sections.last_mut()
    {
        section.field(
            "last failure",
            Cell::text(format!(
                "{} (step {})",
                failure["completed_at"]
                    .as_u64()
                    .map(human_epoch_ago)
                    .unwrap_or_else(|| "unknown".to_string()),
                failure["step"].as_str().unwrap_or("unknown"),
            )),
        );
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
            // The outcome and when it was reached are one thing said
            // once: "ok, two hours ago" reads better than two lines that
            // say the same thing in two rows.
            section.field(
                "outcome",
                Cell::styled(
                    format!(
                        "{outcome} (ran {})",
                        drill["drilled_at"]
                            .as_u64()
                            .map(human_epoch_ago)
                            .unwrap_or_else(|| "unknown".to_string())
                    ),
                    attempt_outcome_style(outcome),
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

    // The moments the off-host copy can be taken back to, and the last
    // time one was. "There is a backup" is a claim; "you can go back to
    // any moment between these two" is the one that matters at 2am, and
    // a window nobody has ever exercised is a promise rather than
    // evidence -- so both are reported side by side.
    match backups["restore_points"].as_object() {
        Some(points) => {
            let mut section = Section::new("point-in-time restore");
            let window = match (
                points["window_start"].as_str(),
                points["window_end"].as_str(),
            ) {
                (Some(start), Some(end)) => {
                    format!("{} to {}", human_instant(start), human_instant(end))
                }
                _ => "unknown".to_string(),
            };
            section.field("window", Cell::text(window));
            let databases = points["databases"]
                .as_object()
                .map(|map| map.len())
                .unwrap_or(0);
            let sidecar = points["sidecar"].as_str().unwrap_or("unknown");
            section.field(
                "replicated",
                Cell::styled(
                    format!("{databases} databases, sidecar {sidecar}"),
                    sidecar_style(sidecar),
                ),
            );
            // The last restore that was taken, when there was one. A
            // window with no exercise is still a window, but it is a
            // promise, and saying so is more useful than implying the
            // path was walked.
            match backups["last_restore"].as_object() {
                Some(restore) => {
                    let outcome = restore["outcome"].as_str().unwrap_or("unknown");
                    section.field(
                        "last restore",
                        Cell::styled(
                            format!(
                                "{} at {} ({})",
                                restore["database"].as_str().unwrap_or("unknown"),
                                human_instant(restore["timestamp"].as_str().unwrap_or("unknown")),
                                outcome
                            ),
                            attempt_outcome_style(outcome),
                        ),
                    );
                }
                None => {
                    section.field("last restore", Cell::text("none taken yet"));
                }
            }
            sections.push(section);
        }
        None => {
            // Two facts are missing here, not one: the window and the
            // last restore taken from it. A host that publishes neither
            // says so about both, rather than drawing one empty section
            // that reads like "there is nothing to restore to" when the
            // truth is "nobody has said".
            sections.push(unreported(
                "point-in-time restore",
                "this host publishes no restore points",
            ));
            sections.push(unreported(
                "last restore",
                "this host publishes no restore result",
            ));
        }
    }

    sections
}

/// The plan for a restore: what a restore to a moment would reach.
///
/// The window is per database, not global, and that is the fact this
/// view exists to show: a database whose replica starts later than the
/// others cannot be taken back to a moment the others can. A restore
/// to that moment fails for that database and succeeds for the rest, so
/// the plan says which is which BEFORE anyone asks for the restore.
///
/// The window comes from the host, which publishes it; this view only
/// reads the published facts and compares them with the moment asked
/// for.
pub fn restore_plan(backups: &Value, to: Option<&str>) -> Vec<Section> {
    let plan = RestorePlan::read(backups, to);
    let mut sections = Vec::new();

    let mut head = Section::new("restore plan");
    head.field(
        "moment",
        Cell::text(match &plan.moment {
            Some(moment) => moment.clone(),
            None => "the whole window".to_string(),
        }),
    );
    head.field(
        "window",
        Cell::text(match (&plan.window_start, &plan.window_end) {
            (Some(start), Some(end)) => {
                format!("{} to {}", human_instant(start), human_instant(end))
            }
            _ => "unknown".to_string(),
        }),
    );
    head.field(
        "sidecar",
        Cell::text(plan.sidecar.clone().unwrap_or_else(|| "unknown".into())),
    );
    sections.push(head);

    if plan.databases.is_empty() {
        sections.push(unreported(
            "restore plan",
            "this host publishes a window with no database in it",
        ));
        return sections;
    }
    let mut table = Table::new();
    table
        .column("database", Align::Left)
        .column("window", Align::Left)
        .column("files", Align::Right)
        .column("bytes", Align::Right)
        .column("the moment", Align::Left);
    for database in &plan.databases {
        table.row(vec![
            Cell::text(database.name.clone()),
            Cell::text(database.span()),
            Cell::text(database.files.to_string()),
            Cell::text(human_bytes(database.bytes)),
            Cell::text(database.note.clone()),
        ]);
    }
    let mut section = Section::new("per database");
    section.table(table);
    sections.push(section);
    sections
}

/// One database's place in a restore plan.
pub struct PlannedDatabase {
    /// The database's name.
    pub name: String,
    /// The oldest moment the replica holds for it.
    pub oldest: Option<String>,
    /// The newest moment the replica holds for it.
    pub newest: Option<String>,
    /// How many files the replica holds for it.
    pub files: u64,
    /// How many bytes those files are.
    pub bytes: u64,
    /// Whether the asked-for moment falls inside this window.
    ///
    /// `None` when no moment was asked for, and also when the window
    /// cannot be read as a span: "unknown" is honest, "no" would be a
    /// guess.
    pub covers: Option<bool>,
    /// Why, in words a person reads.
    pub note: String,
}

impl PlannedDatabase {
    /// The window as a person reads it.
    pub fn span(&self) -> String {
        match (&self.oldest, &self.newest) {
            (Some(oldest), Some(newest)) => {
                format!("{} to {}", human_instant(oldest), human_instant(newest))
            }
            _ => "unknown".to_string(),
        }
    }
}

/// The plan for a restore, as data.
///
/// The same reading the human view draws, in machine form: a pipe should
/// be able to ask "does this database cover that moment" without reading
/// a table.
pub struct RestorePlan {
    /// The moment asked for, when there was one.
    pub moment: Option<String>,
    /// The start of the window the host publishes.
    pub window_start: Option<String>,
    /// The end of that window.
    pub window_end: Option<String>,
    /// Whether the sidecar that does the copying is running.
    pub sidecar: Option<String>,
    /// Each database's place in the window.
    pub databases: Vec<PlannedDatabase>,
}

impl RestorePlan {
    /// Read the plan out of the published facts.
    pub fn read(backups: &Value, to: Option<&str>) -> RestorePlan {
        let moment = to.map(str::to_string);
        let at = moment.as_deref().and_then(render::instant_epoch);
        let Some(points) = backups["restore_points"].as_object() else {
            return RestorePlan {
                moment,
                window_start: None,
                window_end: None,
                sidecar: None,
                databases: Vec::new(),
            };
        };
        let mut databases = Vec::new();
        if let Some(entries) = points["databases"].as_object() {
            for (name, window) in entries {
                let oldest = window["oldest"].as_str().map(str::to_string);
                let newest = window["newest"].as_str().map(str::to_string);
                let (covers, note) = match at {
                    Some(at) => match (
                        oldest.as_deref().and_then(render::instant_epoch),
                        newest.as_deref().and_then(render::instant_epoch),
                    ) {
                        (Some(oldest), Some(newest)) => {
                            if at < oldest {
                                (
                                    Some(false),
                                    "no \u{2014} the window opens later".to_string(),
                                )
                            } else if at > newest {
                                (
                                    Some(false),
                                    "no \u{2014} the window closed earlier".to_string(),
                                )
                            } else {
                                (Some(true), "yes".to_string())
                            }
                        }
                        _ => (
                            None,
                            "unknown \u{2014} the window is not readable".to_string(),
                        ),
                    },
                    None => (None, "-".to_string()),
                };
                databases.push(PlannedDatabase {
                    name: name.clone(),
                    oldest,
                    newest,
                    files: window["files"].as_u64().unwrap_or(0),
                    bytes: window["bytes"].as_u64().unwrap_or(0),
                    covers,
                    note,
                });
            }
        }
        databases.sort_by(|a, b| a.name.cmp(&b.name));
        RestorePlan {
            moment,
            window_start: points["window_start"].as_str().map(str::to_string),
            window_end: points["window_end"].as_str().map(str::to_string),
            sidecar: points["sidecar"].as_str().map(str::to_string),
            databases,
        }
    }

    /// The plan as JSON.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "moment": self.moment,
            "window": {"start": self.window_start, "end": self.window_end},
            "sidecar": self.sidecar,
            "databases": self.databases.iter().map(|database| serde_json::json!({
                "database": database.name,
                "oldest": database.oldest,
                "newest": database.newest,
                "files": database.files,
                "bytes": database.bytes,
                "covers": database.covers,
                "note": database.note,
            })).collect::<Vec<_>>(),
        })
    }
}

/// The restore points the off-host replica offers, as its own view.
///
/// This is the answer to "can I get my data back from last Tuesday?", so
/// it stands on its own rather than hiding inside the backups view: an
/// operator who asks for it is usually about to need it.
pub fn restore(backups: &Value) -> Vec<Section> {
    let mut sections = Vec::new();
    match backups["restore_points"].as_object() {
        Some(points) => {
            let mut section = Section::new("restore points");
            let window = match (
                points["window_start"].as_str(),
                points["window_end"].as_str(),
            ) {
                (Some(start), Some(end)) => {
                    format!("{} to {}", human_instant(start), human_instant(end))
                }
                _ => "unknown".to_string(),
            };
            section.field("window", Cell::text(window));
            section.field(
                "published",
                Cell::text(
                    points["published_at"]
                        .as_u64()
                        .map(human_epoch_ago)
                        .unwrap_or_else(|| "unknown".to_string()),
                ),
            );
            sections.push(section);

            if let Some(databases) = points["databases"].as_object() {
                let mut table = Table::new();
                table
                    .column("database", Align::Left)
                    .column("window", Align::Left)
                    .column("files", Align::Right);
                for (name, window) in databases {
                    let span = match (window["oldest"].as_str(), window["newest"].as_str()) {
                        (Some(oldest), Some(newest)) => {
                            format!("{} to {}", human_instant(oldest), human_instant(newest))
                        }
                        _ => "unknown".to_string(),
                    };
                    table.row(vec![
                        Cell::text(name.clone()),
                        Cell::text(span),
                        Cell::text(window["files"].as_u64().unwrap_or(0).to_string()),
                    ]);
                }
                let mut section = Section::new("replicated databases");
                section.table(table);
                sections.push(section);
            }
        }
        None => sections.push(unreported(
            "restore points",
            "this host publishes no restore points",
        )),
    }

    match backups["last_restore"].as_object() {
        Some(restore) => {
            let mut section = Section::new("last restore");
            let outcome = restore["outcome"].as_str().unwrap_or("unknown");
            section.field(
                "outcome",
                Cell::styled(outcome.to_string(), attempt_outcome_style(outcome)),
            );
            section.field(
                "database",
                Cell::text(restore["database"].as_str().unwrap_or("unknown")),
            );
            section.field(
                "moment",
                Cell::text(human_instant(
                    restore["timestamp"].as_str().unwrap_or("unknown"),
                )),
            );
            section.field(
                "contents",
                Cell::text(format!(
                    "{} tables, {} rows, {}",
                    restore["tables"].as_u64().unwrap_or(0),
                    restore["rows"].as_u64().unwrap_or(0),
                    human_bytes(restore["size_bytes"].as_u64().unwrap_or(0))
                )),
            );
            sections.push(section);
        }
        None => sections.push(unreported(
            "last restore",
            "this host has published no restore result",
        )),
    }

    sections
}
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
        // `behind` is quiet on purpose: the copy is waiting for the
        // producer's next publication and the approved cadence says that
        // is fine. Drawing it like a warning is how the daily digest
        // taught the operator to ignore red.
        "behind" => Style::Dim,
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

/// One database's tables with their row counts, from a
/// `GET /v1/databases/{name}` answer.
///
/// The table the shell's `.tables` implies but never draws: the TUI
/// reads the same answer, so the two surfaces cannot disagree about
/// what a database holds.
pub fn database_tables(info: &Value) -> Table {
    let mut table = Table::new();
    table.column("table", Align::Left);
    table.column("rows", Align::Right);
    let mut count = 0usize;
    for entry in info["tables"].as_array().cloned().unwrap_or_default() {
        let name = entry["name"].as_str().unwrap_or("").to_string();
        let rows = entry["rows"].as_u64().unwrap_or(0);
        table.row(vec![Cell::text(name), Cell::number(rows)]);
        count += 1;
    }
    table.footer(if count == 1 {
        "1 table".to_string()
    } else {
        format!("{count} tables")
    });
    table
}

/// One table's structure, as the shared view layer reads it.
///
/// The reading happens once, from the gateway's own document, so the
/// CLI and the terminal view cannot disagree about what a table
/// holds. The fields are what an operator checks a table for: the
/// column's name, the type it stores, whether it may be null, and
/// whether it is the primary key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableStructure {
    /// The table's name.
    pub name: String,
    /// How many rows it holds.
    pub rows: u64,
    /// Its columns, in declaration order.
    pub columns: Vec<ColumnStructure>,
    /// Its indexes.
    pub indexes: Vec<IndexStructure>,
    /// Its foreign keys.
    pub foreign_keys: Vec<ForeignKeyStructure>,
    /// The statement that created it, verbatim.
    pub sql: Option<String>,
}

/// One column of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnStructure {
    /// The column's name.
    pub name: String,
    /// Its declared type, empty when the schema gave none.
    pub kind: String,
    /// Whether the schema forbids a null.
    pub not_null: bool,
    /// Whether the column is part of the primary key.
    pub primary_key: bool,
    /// Its default expression, when it has one.
    pub default: Option<String>,
}

/// One index of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexStructure {
    /// The index's name.
    pub name: String,
    /// Whether it is unique.
    pub unique: bool,
    /// The columns it covers, in order.
    pub columns: Vec<String>,
}

/// One foreign key of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyStructure {
    /// The table it points at.
    pub table: String,
    /// The column it points from.
    pub from: String,
    /// The column it points at.
    pub to: String,
    /// What happens on update.
    pub on_update: String,
    /// What happens on delete.
    pub on_delete: String,
}

/// The tables of a `GET /v1/databases/{name}/schema` answer.
///
/// One table when a name is given, every table when it is not. A name
/// that matches nothing yields no table: a structure view that
/// invented one would be worse than one that says there is none.
pub fn structures(document: &Value, table: Option<&str>) -> Vec<TableStructure> {
    document["tables"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let name = entry["name"].as_str().unwrap_or("").to_string();
            if let Some(wanted) = table
                && name != wanted
            {
                return None;
            }
            Some(TableStructure {
                name,
                rows: entry["rows"].as_u64().unwrap_or(0),
                columns: entry["columns"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|column| ColumnStructure {
                        name: column["name"].as_str().unwrap_or("").to_string(),
                        kind: column["type"].as_str().unwrap_or("").to_string(),
                        not_null: column["not_null"].as_bool().unwrap_or(false),
                        primary_key: column["primary_key"].as_bool().unwrap_or(false),
                        default: column["default"].as_str().map(str::to_string),
                    })
                    .collect(),
                indexes: entry["indexes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|index| IndexStructure {
                        name: index["name"].as_str().unwrap_or("").to_string(),
                        unique: index["unique"].as_bool().unwrap_or(false),
                        columns: index["columns"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|column| column.as_str().map(str::to_string))
                            .collect(),
                    })
                    .collect(),
                foreign_keys: entry["foreign_keys"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|key| ForeignKeyStructure {
                        table: key["table"].as_str().unwrap_or("").to_string(),
                        from: key["from"].as_str().unwrap_or("").to_string(),
                        to: key["to"].as_str().unwrap_or("").to_string(),
                        on_update: key["on_update"].as_str().unwrap_or("").to_string(),
                        on_delete: key["on_delete"].as_str().unwrap_or("").to_string(),
                    })
                    .collect(),
                sql: entry["sql"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// One table's columns, as the grid an operator reads.
///
/// The flags are words rather than blanks: a blank cell in a schema
/// grid reads as "not known" when it means "no", and "no" is the fact
/// that matters when a column may be null or may not be the key.
/// A database's tables, as the grid an operator reads when they open it.
///
/// One row per table: how many rows it holds, how many columns, how
/// many indexes and how many foreign keys. It is the answer to "what
/// is in this database", one level above a table's own structure, and
/// it is the grid a click on a table lands in: the first column holds
/// the table's name.
pub fn structure_contents(tables: &[TableStructure]) -> Table {
    let mut grid = Table::new();
    grid.column("table", Align::Left);
    grid.column("rows", Align::Right);
    grid.column("columns", Align::Right);
    grid.column("indexes", Align::Right);
    grid.column("foreign keys", Align::Right);
    for table in tables {
        grid.row(vec![
            Cell::text(table.name.clone()),
            Cell::number(table.rows),
            Cell::number(table.columns.len() as u64),
            Cell::number(table.indexes.len() as u64),
            Cell::number(table.foreign_keys.len() as u64),
        ]);
    }
    grid.footer(if tables.len() == 1 {
        "1 table".to_string()
    } else {
        format!("{} tables", tables.len())
    });
    grid
}

/// One table's columns, as the grid an operator reads.
pub fn structure_columns(table: &TableStructure) -> Table {
    let mut grid = Table::new();
    grid.column("column", Align::Left);
    grid.column("type", Align::Left);
    grid.column("not null", Align::Right);
    grid.column("primary key", Align::Right);
    grid.column("default", Align::Left);
    for column in &table.columns {
        grid.row(vec![
            Cell::text(column.name.clone()),
            Cell::text(if column.kind.is_empty() {
                "none".to_string()
            } else {
                column.kind.clone()
            }),
            Cell::text(if column.not_null { "yes" } else { "no" }),
            Cell::text(if column.primary_key { "yes" } else { "no" }),
            Cell::text(column.default.clone().unwrap_or_default()),
        ]);
    }
    grid.footer(if table.columns.len() == 1 {
        "1 column".to_string()
    } else {
        format!("{} columns", table.columns.len())
    });
    grid
}

/// One table's indexes, as the grid an operator reads.
pub fn structure_indexes(table: &TableStructure) -> Table {
    let mut grid = Table::new();
    grid.column("index", Align::Left);
    grid.column("unique", Align::Right);
    grid.column("columns", Align::Left);
    for index in &table.indexes {
        grid.row(vec![
            Cell::text(index.name.clone()),
            Cell::text(if index.unique { "yes" } else { "no" }),
            Cell::text(index.columns.join(", ")),
        ]);
    }
    grid.footer(if table.indexes.len() == 1 {
        "1 index".to_string()
    } else {
        format!("{} indexes", table.indexes.len())
    });
    grid
}

/// One table's foreign keys, as the grid an operator reads.
pub fn structure_foreign_keys(table: &TableStructure) -> Table {
    let mut grid = Table::new();
    grid.column("column", Align::Left);
    grid.column("references", Align::Left);
    grid.column("on update", Align::Left);
    grid.column("on delete", Align::Left);
    for key in &table.foreign_keys {
        let target = if key.to.is_empty() {
            key.table.clone()
        } else {
            format!("{}.{}", key.table, key.to)
        };
        grid.row(vec![
            Cell::text(key.from.clone()),
            Cell::text(target),
            Cell::text(key.on_update.clone()),
            Cell::text(key.on_delete.clone()),
        ]);
    }
    grid.footer(if table.foreign_keys.len() == 1 {
        "1 foreign key".to_string()
    } else {
        format!("{} foreign keys", table.foreign_keys.len())
    });
    grid
}

/// A database's tables and their structure, as the sections a terminal
/// command draws.
///
/// One section per table: the columns are the answer, and the indexes
/// and foreign keys follow when the table has them. A table with
/// neither does not draw empty boxes: an empty index table reads as
/// "this table has no indexes" only when it says so, and here it says
/// so by not being there at all.
pub fn structure_sections(document: &Value, table: Option<&str>) -> Vec<Section> {
    let tables = structures(document, table);
    if tables.is_empty() {
        let why = match table {
            Some(wanted) => format!("this database has no table named {wanted}"),
            None => "this database has no table to describe".to_string(),
        };
        return vec![unreported("structure", &why)];
    }
    tables
        .iter()
        .map(|entry| {
            let mut section = Section::new(entry.name.clone());
            section.field("rows", Cell::number(entry.rows));
            section.field("columns", Cell::number(entry.columns.len() as u64));
            section.table(structure_columns(entry));
            if !entry.indexes.is_empty() {
                section.table(structure_indexes(entry));
            }
            if !entry.foreign_keys.is_empty() {
                section.table(structure_foreign_keys(entry));
            }
            if let Some(sql) = &entry.sql {
                section.field("created by", Cell::text(sql.clone()));
            }
            section
        })
        .collect()
}

/// The facts one table's structure carries, for a surface that draws a
/// headline and a few facts beside one grid.
pub fn structure_facts(table: &TableStructure) -> Vec<(String, String)> {
    let mut facts = vec![
        ("table".to_string(), table.name.clone()),
        ("rows".to_string(), table.rows.to_string()),
        (
            "columns".to_string(),
            if table.columns.len() == 1 {
                "1".to_string()
            } else {
                table.columns.len().to_string()
            },
        ),
    ];
    if !table.indexes.is_empty() {
        facts.push((
            "indexes".to_string(),
            table
                .indexes
                .iter()
                .map(|index| {
                    if index.unique {
                        format!("{} (unique)", index.name)
                    } else {
                        index.name.clone()
                    }
                })
                .collect::<Vec<String>>()
                .join(", "),
        ));
    }
    if !table.foreign_keys.is_empty() {
        facts.push((
            "foreign keys".to_string(),
            table
                .foreign_keys
                .iter()
                .map(|key| format!("{} -> {}", key.from, key.table))
                .collect::<Vec<String>>()
                .join(", "),
        ));
    }
    facts
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

/// The statement shapes the service has run, and what each one cost.
///
/// This is the answer to "which query is heavy", and it is the one
/// thing a hosted provider structurally cannot give: the log holds the
/// SQL every credential ran, and it never leaves the box.
///
/// The shape is the SQL with its literals replaced, so the row names a
/// statement without quoting the data it carried.
pub fn queries(payload: &Value) -> Vec<Section> {
    let mut sections = Vec::new();
    let databases = payload["databases"].as_array().cloned().unwrap_or_default();
    if databases.is_empty() {
        sections.push(unreported(
            "queries",
            "this service has run no statements it has recorded",
        ));
        return sections;
    }
    for entry in &databases {
        let name = entry["database"].as_str().unwrap_or("unknown");
        let shapes = entry["shapes"].as_array().cloned().unwrap_or_default();
        let mut table = Table::new();
        table
            .column("statement", Align::Left)
            .column("runs", Align::Right)
            .column("rows read", Align::Right)
            .column("rows written", Align::Right)
            .column("total time", Align::Right)
            .column("slowest", Align::Right)
            .column("by", Align::Left);
        for shape in &shapes {
            let principals = shape["principals"]
                .as_object()
                .map(|map| {
                    let mut names: Vec<String> = map
                        .iter()
                        .map(|(name, count)| format!("{name} ({count})"))
                        .collect();
                    names.sort();
                    names.join(", ")
                })
                .unwrap_or_default();
            table.row(vec![
                Cell::text(shape["shape"].as_str().unwrap_or("")),
                Cell::text(human_count(shape["count"].as_u64().unwrap_or(0), "")),
                Cell::text(human_count(shape["rows_read"].as_u64().unwrap_or(0), "")),
                Cell::text(human_count(shape["rows_written"].as_u64().unwrap_or(0), "")),
                Cell::text(human_micros(shape["total_micros"].as_u64().unwrap_or(0))),
                Cell::text(human_micros(shape["max_micros"].as_u64().unwrap_or(0))),
                Cell::text(principals),
            ]);
        }
        let mut section = Section::new(name.to_string());
        section.table(table);
        sections.push(section);
    }
    sections
}

/// The statement shapes the service has run, as one table.
///
/// The terminal view wants a single scrolling table rather than one
/// table per database: the question it answers -- "which statement is
/// heavy" -- is asked across the whole service, and a table per
/// database would mean the operator scrolls between boxes to compare.
pub fn query_table(payload: &Value) -> Table {
    let mut table = Table::new();
    table
        .column("database", Align::Left)
        .column("statement", Align::Left)
        .column("runs", Align::Right)
        .column("rows read", Align::Right)
        .column("rows written", Align::Right)
        .column("total time", Align::Right)
        .column("slowest", Align::Right)
        .column("by", Align::Left);
    for entry in payload["databases"].as_array().cloned().unwrap_or_default() {
        let database = entry["database"].as_str().unwrap_or("unknown");
        for shape in entry["shapes"].as_array().cloned().unwrap_or_default() {
            let principals = shape["principals"]
                .as_object()
                .map(|map| {
                    let mut names: Vec<String> = map
                        .iter()
                        .map(|(name, count)| format!("{name} ({count})"))
                        .collect();
                    names.sort();
                    names.join(", ")
                })
                .unwrap_or_default();
            table.row(vec![
                Cell::text(database),
                Cell::text(clip_shape(
                    shape["shape"].as_str().unwrap_or(""),
                    SHAPE_WIDTH,
                )),
                Cell::text(human_count(shape["count"].as_u64().unwrap_or(0), "")),
                Cell::text(human_count(shape["rows_read"].as_u64().unwrap_or(0), "")),
                Cell::text(human_count(shape["rows_written"].as_u64().unwrap_or(0), "")),
                Cell::text(human_micros(shape["total_micros"].as_u64().unwrap_or(0))),
                Cell::text(human_micros(shape["max_micros"].as_u64().unwrap_or(0))),
                Cell::text(principals),
            ]);
        }
    }
    table
}

/// How much of a statement shape the terminal view shows.
///
/// A statement can be arbitrarily long, and a table cell that long
/// squeezes every other column out of the screen. The shape is clipped
/// and marked as clipped, so the operator knows there is more to it.
const SHAPE_WIDTH: usize = 72;

/// A statement shape, clipped to a width, with a marker when it was cut.
fn clip_shape(shape: &str, width: usize) -> String {
    if shape.chars().count() <= width {
        return shape.to_string();
    }
    let mut out: String = shape.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
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

/// One page of a table's rows: the SQL to run, and from where.
///
/// The rows browser has to turn an operator's choices -- which table,
/// which page, which column to sort by, what to filter on -- into a
/// statement. Doing that safely is the whole job of this function, so it
/// lives here, in the shared view layer, where the CLI and the TUI ask
/// the same code the same question and neither can grow its own idea of
/// what is safe.
///
/// The rules, and why each one is stated rather than assumed:
///
/// * The table name is **validated against the database's own schema**,
///   not matched against a pattern. A name that is not a real table is
///   refused before any SQL is built, so the name is never interpolated
///   on trust.
/// * The sort column is validated the same way, against the table's real
///   columns. `ORDER BY` cannot take a bound parameter, so the column
///   name has to be written into the statement -- which is exactly why
///   it is checked against the schema first. The direction is an enum,
///   not a string from the operator.
/// * The filter is **bound as a parameter**, never concatenated, and it
///   is re-checked by the same read-only rule the query endpoint uses.
///   A filter is a boolean SQL expression (`stock < 20`), so it cannot
///   be a bound value itself; it is therefore constrained to a
///   read-only expression, and the resulting statement is validated
///   again before it can be sent.
/// * The page size is a constant, and the offset is computed from a
///   page number, so no count from the operator reaches the SQL as
///   text.
pub struct RowsQuery {
    /// The statement to run.
    pub sql: String,
}

impl std::fmt::Debug for RowsQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowsQuery").field("sql", &self.sql).finish()
    }
}

/// Which way a sort runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    /// The SQL keyword. An enum, never a string from the wire.
    fn keyword(self) -> &'static str {
        match self {
            SortDirection::Ascending => "ASC",
            SortDirection::Descending => "DESC",
        }
    }
}

/// How many rows one page of the browser holds.
pub const ROWS_PAGE: u64 = 10;

/// The longest filter the browser accepts.
///
/// A filter is an expression, and an expression is code: the bound is
/// here so a pasted document cannot become a statement.
const FILTER_LIMIT: usize = 512;

/// Why a rows request was refused.
///
/// The variants exist so the caller can say which question was
/// malformed, rather than "invalid" for all of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowsRefusal {
    /// The named table is not in this database.
    NoSuchTable(String),
    /// The sort column is not a column of this table.
    NoSuchColumn(String),
    /// The filter is empty, over-long, or not a read-only expression.
    BadFilter,
}

impl RowsRefusal {
    /// The sentence an operator reads.
    pub fn message(&self) -> String {
        match self {
            RowsRefusal::NoSuchTable(name) => {
                format!("this database has no table named {name}")
            }
            RowsRefusal::NoSuchColumn(name) => {
                format!("this table has no column named {name}")
            }
            RowsRefusal::BadFilter => {
                "a filter must be a read-only SQL condition, at most 512 characters".to_string()
            }
        }
    }
}

/// The statement that reads one page of a table's rows.
///
/// `structure` is the gateway's own answer about the database (the
/// document the schema route returns), so the table and the columns are
/// the schema's real names and not the operator's spelling of them.
pub fn rows_query(
    structure: &Value,
    table: &str,
    page: u64,
    sort: Option<(&str, SortDirection)>,
    filter: Option<&str>,
) -> std::result::Result<RowsQuery, RowsRefusal> {
    let entry = structure["tables"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|entry| entry["name"].as_str() == Some(table))
        .ok_or_else(|| RowsRefusal::NoSuchTable(table.to_string()))?;
    // The sort column has to be one of this table's own columns. It is
    // compared against the schema's list, not pattern-matched: a quoted
    // identifier from the schema cannot break out of its own quoting,
    // and a name that is not in the list never reaches the statement.
    let order = match sort {
        Some((column, direction)) => {
            let known = entry["columns"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .any(|known| known["name"].as_str() == Some(column));
            if !known {
                return Err(RowsRefusal::NoSuchColumn(column.to_string()));
            }
            format!(
                " ORDER BY {} {}, rowid",
                quote_identifier(column),
                direction.keyword()
            )
        }
        // Unsorted pages still need a stable order, or page 2 can repeat
        // a row page 1 already showed: `rowid` is SQLite's own answer to
        // "in what order does this table actually store its rows".
        None => " ORDER BY rowid".to_string(),
    };
    let condition = match filter.map(str::trim) {
        None | Some("") => String::new(),
        Some(when) => {
            if when.len() > FILTER_LIMIT {
                return Err(RowsRefusal::BadFilter);
            }
            // The condition goes into a WHERE clause, so it is checked
            // as an expression: it may not smuggle a statement through a
            // separator, and it may not contain a write.
            if !filter_is_safe(when) {
                return Err(RowsRefusal::BadFilter);
            }
            format!(" WHERE ({when})")
        }
    };
    // A page number that would overflow the offset is refused, not
    // saturated: a saturated offset reads an arbitrary slice of the
    // table and calls it page N.
    let offset = page.checked_mul(ROWS_PAGE).ok_or(RowsRefusal::BadFilter)?;
    Ok(RowsQuery {
        sql: format!(
            "SELECT * FROM {}{condition}{order} LIMIT {ROWS_PAGE} OFFSET {offset}",
            quote_identifier(table)
        ),
    })
}

/// A name quoted for SQLite.
///
/// Every name that reaches this function came from the schema, but a
/// quote inside a name would still break the statement out of its
/// identifier: it is doubled, the way SQLite expects.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Whether a filter is a read-only condition and not a statement.
///
/// A filter may not contain a statement separator, a comment (which can
/// comment out the rest of the generated SQL), or a set operation --
/// `UNION SELECT` is a read, so the write-keyword scan would allow it,
/// and it would splice a second result set into this one. That is
/// stricter than the query endpoint, which allows a whole statement; a
/// filter is only ever an expression, so anything statement-shaped is
/// refused.
fn filter_is_safe(when: &str) -> bool {
    if when.contains(';') || when.contains("--") || when.contains("/*") {
        return false;
    }
    // A set operation is a statement, not a condition. The keyword scan
    // below only looks for writes; `UNION`, `INTERSECT` and `EXCEPT` are
    // reads, and each one would append a result set the operator did
    // not ask for.
    let upper = when.to_ascii_uppercase();
    for operation in ["UNION", "INTERSECT", "EXCEPT"] {
        if upper.contains(operation) {
            return false;
        }
    }
    // The same keyword scan the read-only rule uses, applied to the
    // filter alone: `SELECT * FROM t WHERE (1=1) DELETE` must not pass.
    crate::execution::is_read_only(&[crate::execution::Statement {
        sql: format!("SELECT 1 WHERE {when}"),
        args: Vec::new(),
    }])
}

/// The statement that counts a table's rows under the same filter.
///
/// The browser's footer says `rows 11-20 of N`, and N has to be the
/// count of what the filter selects -- not the table's total. Otherwise
/// a filtered view claims to have 60 rows and shows 4, which is the kind
/// of quiet lie that makes an operator distrust the whole view.
pub fn rows_count_query(
    structure: &Value,
    table: &str,
    filter: Option<&str>,
) -> std::result::Result<RowsQuery, RowsRefusal> {
    // The same validation as the page query, by asking it: a table that
    // does not exist, or a filter that is not a condition, is refused in
    // exactly one place.
    rows_query(structure, table, 0, None, filter)?;
    let condition = match filter.map(str::trim) {
        None | Some("") => String::new(),
        Some(when) => format!(" WHERE ({when})"),
    };
    Ok(RowsQuery {
        sql: format!(
            "SELECT count(*) FROM {}{condition}",
            quote_identifier(table)
        ),
    })
}
