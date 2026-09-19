//! Dummy fleet: task tracker (projects / tasks / comments).
//!
//! Run: `timeout 300 cargo test --locked --test fleet_tasks`
//!
//! A realistic small-team tracker driven through the real `dg` binary
//! only: FK-chained schema, multi-row seed, join + GROUP BY reads,
//! keyed insert with byte-identical replay (app-retry safety), status
//! update, FK-violation rejection, ordered thread read, and exact
//! finale counts. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

const DB: &str = "fleet_tasks";

fn apply(fleet: &Fleet, sql_file: &str) {
    for stmt in sql_file.split(";\n") {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        fleet.cli.q("seed stmt", DB, stmt);
    }
}

#[test]
fn task_tracker_works_like_a_real_app() {
    let fleet = Fleet::boot();
    assert_eq!(fleet.cli.ok("create", &["create", DB])["name"], DB);
    apply(&fleet, include_str!("../fleet/task_tracker_schema.sql"));
    apply(&fleet, include_str!("../fleet/task_tracker_seed.sql"));

    // Open (non-done) task counts per live project, joined.
    assert_eq!(
        fleet.cli.q(
            "open per project",
            DB,
            "SELECT p.name, count(t.id) FROM projects p LEFT JOIN tasks t ON t.project_id = p.id AND t.status != 'done' WHERE p.archived = 0 GROUP BY p.name ORDER BY p.name"
        ),
        json!([["Gateway bridge", 3], ["Website relaunch", 3]])
    );

    // Status rollup: 4 done, 2 in_progress, 6 open.
    assert_eq!(
        fleet.cli.q(
            "status rollup",
            DB,
            "SELECT status, count(*) FROM tasks GROUP BY status ORDER BY status"
        ),
        json!([["done", 4], ["in_progress", 2], ["open", 6]])
    );

    // Ana's open work ordered by priority.
    assert_eq!(
        fleet.cli.q(
            "assignee filter",
            DB,
            "SELECT title FROM tasks WHERE assignee = 'ana' AND status != 'done' ORDER BY priority, id"
        ),
        json!([["Build photo gallery"], ["Write launch post"]])
    );

    // Comment thread on the replay drill, in order.
    assert_eq!(
        fleet.cli.q(
            "thread",
            DB,
            "SELECT author, body FROM comments WHERE task_id = 6 ORDER BY id"
        ),
        json!([
            ["dan", "First replay drill passed"],
            ["cara", "Second drill covers restarts"],
            ["dan", "Restart drill passed too"]
        ])
    );

    // App retry: same keyed insert twice is one row, byte-identical.
    let first = fleet.cli.qkey(
        "keyed task",
        DB,
        "INSERT INTO tasks(project_id, title, status, priority, assignee) VALUES (2, 'Load test Tuesday', 'open', 2, 'dan')",
        "fleet-tasks-1",
    );
    let replay = fleet.cli.qkey(
        "keyed task replay",
        DB,
        "INSERT INTO tasks(project_id, title, status, priority, assignee) VALUES (2, 'Load test Tuesday', 'open', 2, 'dan')",
        "fleet-tasks-1",
    );
    assert_eq!(first, replay, "keyed replay bytes drifted");
    assert_eq!(
        fleet.cli.q("task count", DB, "SELECT count(*) FROM tasks"),
        json!([[13]])
    );

    // Ship it: status update moves the rollup exactly.
    fleet.cli.q("mark done", DB, "UPDATE tasks SET status = 'done' WHERE id = 13");
    assert_eq!(
        fleet.cli.q(
            "rollup after ship",
            DB,
            "SELECT status, count(*) FROM tasks GROUP BY status ORDER BY status"
        ),
        json!([["done", 5], ["in_progress", 2], ["open", 6]])
    );

    // Dangling references are rejected, never stored.
    fleet.cli.err(
        "task bad project",
        &["query", DB, "INSERT INTO tasks(project_id, title) VALUES (999, 'ghost')"],
        "invalid_sql",
    );
    fleet.cli.err(
        "comment bad task",
        &[
            "query",
            DB,
            "INSERT INTO comments(task_id, author, body) VALUES (999, 'x', 'ghost')",
        ],
        "invalid_sql",
    );

    // Finale: everything exact.
    assert_eq!(
        fleet.cli.q("projects", DB, "SELECT count(*) FROM projects"),
        json!([[3]])
    );
    assert_eq!(
        fleet.cli.q("tasks final", DB, "SELECT count(*) FROM tasks"),
        json!([[13]])
    );
    assert_eq!(
        fleet.cli.q("comments", DB, "SELECT count(*) FROM comments"),
        json!([[8]])
    );
}
