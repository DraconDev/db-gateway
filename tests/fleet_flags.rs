//! Dummy fleet: feature flags / settings KV (upserts, keyed writes).
//!
//! Run: `timeout 300 cargo test --locked --test fleet_flags`
//!
//! A tiny config service driven through the real `dg` binary only:
//! point lookups, full dump, prefix scan, toggle with audit row,
//! ON CONFLICT upserts (existing and new), a keyed flag create
//! stored once byte-identical, flag retirement, and exact finale
//! counts. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

const DB: &str = "fleet_flags";

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
fn flags_serve_like_a_real_config_service() {
    let fleet = Fleet::boot();
    assert_eq!(fleet.cli.ok("create", &["create", DB])["name"], DB);
    apply(&fleet, include_str!("../fleet/flags_schema.sql"));
    apply(&fleet, include_str!("../fleet/flags_seed.sql"));

    // Point lookup.
    assert_eq!(
        fleet
            .cli
            .q("get", DB, "SELECT value FROM flags WHERE key = 'dark.mode'"),
        json!([["off"]])
    );

    // A text primary key remains addressable: SQLite's legacy nullable
    // PRIMARY KEY behavior must not admit unaddressable NULL configuration keys.
    fleet.cli.err(
        "reject null flag key",
        &[
            "query",
            DB,
            "INSERT INTO flags(key, value, updated_at) VALUES(NULL, 'bad', 'now')",
        ],
        "invalid_sql",
    );

    // Full dump in key order.
    assert_eq!(
        fleet
            .cli
            .q("dump", DB, "SELECT key, value FROM flags ORDER BY key"),
        json!([
            ["api.rate_limit", "1000"],
            ["checkout.v2", "on"],
            ["dark.mode", "off"],
            ["homepage.hero", "variant-b"],
            ["maintenance", "off"],
            ["signup.captcha", "on"]
        ])
    );

    // Prefix scan for one feature family.
    assert_eq!(
        fleet.cli.q(
            "prefix",
            DB,
            "SELECT key, value FROM flags WHERE key LIKE 'checkout.%' ORDER BY key"
        ),
        json!([["checkout.v2", "on"]])
    );

    // Toggle with an audit row.
    fleet.cli.q(
        "toggle",
        DB,
        "UPDATE flags SET value = 'on', updated_at = '2026-03-01T09:00:00' WHERE key = 'dark.mode'",
    );
    fleet.cli.q(
        "audit toggle",
        DB,
        "INSERT INTO flag_changes(key, old_value, new_value, changed_at) VALUES ('dark.mode', 'off', 'on', '2026-03-01T09:00:00')",
    );
    assert_eq!(
        fleet.cli.q(
            "get toggled",
            DB,
            "SELECT value FROM flags WHERE key = 'dark.mode'"
        ),
        json!([["on"]])
    );

    // Upsert over an existing flag keeps one row and logs the change.
    fleet.cli.q(
        "upsert existing",
        DB,
        "INSERT INTO flags(key, value, updated_at) VALUES ('api.rate_limit', '2000', '2026-03-01T10:00:00') ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    );
    fleet.cli.q(
        "audit upsert",
        DB,
        "INSERT INTO flag_changes(key, old_value, new_value, changed_at) VALUES ('api.rate_limit', '1000', '2000', '2026-03-01T10:00:00')",
    );
    assert_eq!(
        fleet.cli.q(
            "one row",
            DB,
            "SELECT count(*) FROM flags WHERE key = 'api.rate_limit'"
        ),
        json!([[1]])
    );
    assert_eq!(
        fleet.cli.q(
            "new limit",
            DB,
            "SELECT value FROM flags WHERE key = 'api.rate_limit'"
        ),
        json!([["2000"]])
    );

    // Upsert of a brand-new flag inserts it and logs the change.
    fleet.cli.q(
        "upsert new",
        DB,
        "INSERT INTO flags(key, value, updated_at) VALUES ('search.v2', 'off', '2026-03-01T11:00:00') ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    );
    fleet.cli.q(
        "audit upsert new",
        DB,
        "INSERT INTO flag_changes(key, old_value, new_value, changed_at) VALUES ('search.v2', NULL, 'off', '2026-03-01T11:00:00')",
    );
    assert_eq!(
        fleet.cli.q(
            "new flag",
            DB,
            "SELECT value FROM flags WHERE key = 'search.v2'"
        ),
        json!([["off"]])
    );

    // Admin double-click: same keyed create twice is one row, byte-identical.
    let first = fleet.cli.qkey(
        "keyed flag",
        DB,
        "INSERT INTO flags(key, value, updated_at) VALUES ('beta.banner', 'on', '2026-03-01T12:00:00')",
        "fleet-flags-1",
    );
    let replay = fleet.cli.qkey(
        "keyed flag replay",
        DB,
        "INSERT INTO flags(key, value, updated_at) VALUES ('beta.banner', 'on', '2026-03-01T12:00:00')",
        "fleet-flags-1",
    );
    assert_eq!(first, replay, "keyed replay bytes drifted");

    // Retire a flag; it reads back empty.
    fleet
        .cli
        .q("retire", DB, "DELETE FROM flags WHERE key = 'maintenance'");
    assert_eq!(
        fleet.cli.q(
            "retired",
            DB,
            "SELECT value FROM flags WHERE key = 'maintenance'"
        ),
        json!([])
    );

    // Finale: 6 seeded + search.v2 + beta.banner - maintenance = 7 flags,
    // 2 seeded + toggle + 2 upserts = 5 audit rows (keyed create is deduped client-side).
    assert_eq!(
        fleet.cli.q("flags", DB, "SELECT count(*) FROM flags"),
        json!([[7]])
    );
    assert_eq!(
        fleet
            .cli
            .q("changes", DB, "SELECT count(*) FROM flag_changes"),
        json!([[5]])
    );
    assert_eq!(
        fleet.cli.q(
            "latest change",
            DB,
            "SELECT key, old_value, new_value FROM flag_changes ORDER BY id DESC LIMIT 1"
        ),
        json!([["search.v2", null, "off"]])
    );
}
