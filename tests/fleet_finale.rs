//! Dummy fleet finale: six project DBs live side by side.
//!
//! Run: `timeout 300 cargo test --locked --test fleet_finale`
//!
//! One server hosts all six fleet databases at once: every DB answers
//! its signature query exactly (cross-fleet check), `dg list` shows
//! exactly the six, a write to one DB leaves the others untouched
//! (isolation), and the shop DB survives an export/import round-trip
//! with byte-identical aggregates. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

fn apply(fleet: &Fleet, db: &str, sql_file: &str) {
    for stmt in sql_file.split(";\n") {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        fleet.cli.q("seed stmt", db, stmt);
    }
}

const DBS: [&str; 6] = [
    "fleet_tasks",
    "fleet_blog",
    "fleet_shop",
    "fleet_analytics",
    "fleet_chat",
    "fleet_flags",
];

#[test]
fn fleet_lives_side_by_side() {
    let fleet = Fleet::boot();
    for db in DBS {
        assert_eq!(fleet.cli.ok("create", &["create", db])["name"], db);
    }
    apply(&fleet, "fleet_tasks", include_str!("../fleet/task_tracker_schema.sql"));
    apply(&fleet, "fleet_tasks", include_str!("../fleet/task_tracker_seed.sql"));
    apply(&fleet, "fleet_blog", include_str!("../fleet/blog_schema.sql"));
    apply(&fleet, "fleet_blog", include_str!("../fleet/blog_seed.sql"));
    apply(&fleet, "fleet_shop", include_str!("../fleet/shop_schema.sql"));
    apply(&fleet, "fleet_shop", include_str!("../fleet/shop_seed.sql"));
    apply(&fleet, "fleet_analytics", include_str!("../fleet/analytics_schema.sql"));
    apply(&fleet, "fleet_analytics", include_str!("../fleet/analytics_seed.sql"));
    apply(&fleet, "fleet_chat", include_str!("../fleet/chat_schema.sql"));
    apply(&fleet, "fleet_chat", include_str!("../fleet/chat_seed.sql"));
    apply(&fleet, "fleet_flags", include_str!("../fleet/flags_schema.sql"));
    apply(&fleet, "fleet_flags", include_str!("../fleet/flags_seed.sql"));

    // Cross-fleet check: every DB answers its signature query exactly.
    assert_eq!(
        fleet.cli.q(
            "tasks rollup",
            "fleet_tasks",
            "SELECT status, count(*) FROM tasks GROUP BY status ORDER BY status"
        ),
        json!([["done", 4], ["in_progress", 2], ["open", 6]])
    );
    assert_eq!(
        fleet.cli.q(
            "blog homepage",
            "fleet_blog",
            "SELECT slug FROM posts WHERE status = 'published' ORDER BY published_at DESC"
        ),
        json!([["soak-report"], ["rust-notes"], ["gateway-bridge"], ["hello-world"], ["old-news"]])
    );
    assert_eq!(
        fleet.cli.q(
            "shop revenue",
            "fleet_shop",
            "SELECT o.status, sum(li.qty * li.unit_price_cents) FROM orders o JOIN line_items li ON li.order_id = o.id GROUP BY o.status ORDER BY o.status"
        ),
        json!([["cancelled", 1999], ["open", 4999], ["paid", 23888]])
    );
    assert_eq!(
        fleet.cli.q(
            "analytics funnel",
            "fleet_analytics",
            "SELECT name, count(*) FROM events GROUP BY name ORDER BY name"
        ),
        json!([["heartbeat", 3], ["page_view", 20], ["purchase", 3], ["signup", 4]])
    );
    assert_eq!(
        fleet.cli.q(
            "chat counts",
            "fleet_chat",
            "SELECT r.name, count(m.id) FROM rooms r LEFT JOIN messages m ON m.room_id = r.id GROUP BY r.name ORDER BY r.name"
        ),
        json!([["gateway-ops", 4], ["general", 3], ["random", 5]])
    );
    assert_eq!(
        fleet.cli.q("flags count", "fleet_flags", "SELECT count(*) FROM flags"),
        json!([[6]])
    );

    // Registry shows exactly the six fleet DBs.
    let listed = fleet.cli.ok("list", &["list"]);
    let mut names: Vec<String> = listed["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "fleet_analytics",
            "fleet_blog",
            "fleet_chat",
            "fleet_flags",
            "fleet_shop",
            "fleet_tasks"
        ]
    );

    // Isolation: a write to tasks leaves the blog untouched.
    fleet.cli.q(
        "extra task",
        "fleet_tasks",
        "INSERT INTO tasks(project_id, title) VALUES (1, 'Side quest')",
    );
    assert_eq!(
        fleet.cli.q("tasks now", "fleet_tasks", "SELECT count(*) FROM tasks"),
        json!([[13]])
    );
    assert_eq!(
        fleet.cli.q("blog still", "fleet_blog", "SELECT count(*) FROM posts"),
        json!([[6]])
    );

    // Export/import round-trip: the shop copy aggregates identically.
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("shop.sqlite");
    let exported = fleet.cli.ok("export", &["export", "fleet_shop", "--out", snap.to_str().unwrap()]);
    assert!(exported["size"].as_u64().unwrap() > 0);
    assert!(snap.exists(), "export wrote no file");
    assert_eq!(
        fleet.cli.ok("import", &["import", "fleet_shop_copy", "--from", snap.to_str().unwrap()])["imported"],
        "fleet_shop_copy"
    );
    for (label, sql) in [
        (
            "copy revenue",
            "SELECT o.status, sum(li.qty * li.unit_price_cents) FROM orders o JOIN line_items li ON li.order_id = o.id GROUP BY o.status ORDER BY o.status",
        ),
        (
            "copy sellers",
            "SELECT p.name, sum(li.qty) FROM products p JOIN line_items li ON li.product_id = p.id GROUP BY p.name ORDER BY sum(li.qty) DESC, p.name",
        ),
    ] {
        assert_eq!(
            fleet.cli.q(label, "fleet_shop_copy", sql),
            fleet.cli.q(label, "fleet_shop", sql),
            "{label} drifted across export/import"
        );
    }

    // Registry now shows the six plus the copy.
    let listed = fleet.cli.ok("list again", &["list"]);
    let mut names: Vec<String> = listed["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "fleet_analytics",
            "fleet_blog",
            "fleet_chat",
            "fleet_flags",
            "fleet_shop",
            "fleet_shop_copy",
            "fleet_tasks"
        ]
    );
}
