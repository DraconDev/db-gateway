//! Dummy fleet: analytics events (append-only log, bulk writes).
//!
//! Run: `timeout 300 cargo test --locked --test fleet_analytics`
//!
//! A tiny event pipeline driven through the real `dg` binary only:
//! daily + per-name rollups, purchase revenue, distinct users, five
//! atomic bulk batches of 32 unkeyed inserts (the high-volume path),
//! a retention purge, a keyed tracker retry stored once, and exact
//! finale counts. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

const DB: &str = "fleet_analytics";

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
fn analytics_rolls_up_like_a_real_pipeline() {
    let fleet = Fleet::boot();
    assert_eq!(fleet.cli.ok("create", &["create", DB])["name"], DB);
    apply(&fleet, include_str!("../fleet/analytics_schema.sql"));
    apply(&fleet, include_str!("../fleet/analytics_seed.sql"));

    // Daily volume: 20 on day one, 10 on day two.
    assert_eq!(
        fleet.cli.q(
            "daily",
            DB,
            "SELECT substr(ts, 1, 10), count(*) FROM events GROUP BY substr(ts, 1, 10) ORDER BY substr(ts, 1, 10)"
        ),
        json!([["2026-03-01", 20], ["2026-03-02", 10]])
    );

    // Funnel: 20 views, 4 signups, 3 purchases, 3 heartbeats.
    assert_eq!(
        fleet.cli.q(
            "funnel",
            DB,
            "SELECT name, count(*) FROM events GROUP BY name ORDER BY name"
        ),
        json!([
            ["heartbeat", 3],
            ["page_view", 20],
            ["purchase", 3],
            ["signup", 4]
        ])
    );

    // Revenue: 5000 + 1299 + 799 = 7098 cents.
    assert_eq!(
        fleet.cli.q(
            "revenue",
            DB,
            "SELECT sum(value_cents) FROM events WHERE name = 'purchase'"
        ),
        json!([[7098]])
    );

    // Nine distinct users; NULL heartbeats do not count.
    assert_eq!(
        fleet
            .cli
            .q("dau", DB, "SELECT count(DISTINCT user_id) FROM events"),
        json!([[9]])
    );

    // Bulk path: five atomic batches of 32 unkeyed inserts land 160 rows.
    let dir = tempfile::tempdir().unwrap();
    for b in 0..5 {
        let mut statements = String::from("{\"atomic\":true,\"statements\":[");
        for i in 0..32 {
            if i > 0 {
                statements.push(',');
            }
            statements.push_str(&format!(
                "{{\"sql\":\"INSERT INTO events(name, user_id, ts) VALUES ('bulk', 'u-bulk', '2026-03-03T12:{b:02}:{i:02}')\"}}"
            ));
        }
        statements.push_str("]}");
        let file = dir.path().join(format!("bulk{b}.json"));
        std::fs::write(&file, statements).unwrap();
        fleet.cli.ok("bulk", &["batch", DB, file.to_str().unwrap()]);
    }
    assert_eq!(
        fleet.cli.q(
            "bulk count",
            DB,
            "SELECT count(*) FROM events WHERE name = 'bulk'"
        ),
        json!([[160]])
    );
    assert_eq!(
        fleet.cli.q("total", DB, "SELECT count(*) FROM events"),
        json!([[190]])
    );

    // Retention purge drops the three heartbeats.
    fleet
        .cli
        .q("purge", DB, "DELETE FROM events WHERE name = 'heartbeat'");
    assert_eq!(
        fleet
            .cli
            .q("after purge", DB, "SELECT count(*) FROM events"),
        json!([[187]])
    );

    // Tracker retry: same keyed purchase twice is one row, byte-identical.
    let first = fleet.cli.qkey(
        "keyed purchase",
        DB,
        "INSERT INTO events(name, user_id, ts, value_cents) VALUES ('purchase', 'u9', '2026-03-03T14:00:00', 2500)",
        "fleet-analytics-1",
    );
    let replay = fleet.cli.qkey(
        "keyed purchase replay",
        DB,
        "INSERT INTO events(name, user_id, ts, value_cents) VALUES ('purchase', 'u9', '2026-03-03T14:00:00', 2500)",
        "fleet-analytics-1",
    );
    assert_eq!(first, replay, "keyed replay bytes drifted");

    // Finale: everything exact.
    assert_eq!(
        fleet
            .cli
            .q("final total", DB, "SELECT count(*) FROM events"),
        json!([[188]])
    );
    assert_eq!(
        fleet.cli.q(
            "final funnel",
            DB,
            "SELECT name, count(*) FROM events GROUP BY name ORDER BY name"
        ),
        json!([
            ["bulk", 160],
            ["page_view", 20],
            ["purchase", 4],
            ["signup", 4]
        ])
    );
    assert_eq!(
        fleet.cli.q(
            "final revenue",
            DB,
            "SELECT sum(value_cents) FROM events WHERE name = 'purchase'"
        ),
        json!([[9598]])
    );
    assert_eq!(
        fleet.cli.q(
            "final users",
            DB,
            "SELECT count(DISTINCT user_id) FROM events"
        ),
        json!([[10]])
    );
}
