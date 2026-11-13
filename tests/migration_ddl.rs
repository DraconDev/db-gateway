//! Schema migration through the gateway: what an application's migration
//! runner does on its own database, end to end through the real `dg`.
//!
//! Run: `timeout 300 cargo test --locked --test migration_ddl`
//!
//! A write credential can ALTER tables (add, rename and drop a column,
//! rename the table), create and drop views and triggers, and read the
//! schema back with the introspection PRAGMAs. A trigger fires on a later
//! write. The migrated database then survives export → import and clone
//! with its views and triggers intact and working, and the schema, info,
//! contents and diff views read it without tripping on the new object
//! kinds. The boundary holds: a RENAME onto a reserved `_dg_` name is
//! refused and rolled back, a trigger body cannot reach `_dg_replay`, and
//! a read-only credential may inspect the schema but not change it.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;

use serde_json::{Value, json};

const DB: &str = "app";

fn q(fleet: &Fleet, sql: &str) -> Value {
    fleet.cli.q(sql, DB, sql)
}

fn forbidden(fleet: &Fleet, db: &str, sql: &str) {
    fleet.cli.err(sql, &["query", db, sql], "sql_forbidden");
}

#[test]
fn migrations_run_and_survive_export_import_and_clone() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", DB]);

    // v1: what the app shipped with.
    q(
        &fleet,
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)",
    );
    q(
        &fleet,
        "INSERT INTO users(name) VALUES ('aria'), ('octavia')",
    );

    // v2: the migration.
    q(&fleet, "ALTER TABLE users ADD COLUMN email TEXT");
    q(&fleet, "ALTER TABLE users ADD COLUMN scratch INTEGER");
    q(&fleet, "ALTER TABLE users DROP COLUMN scratch");
    q(
        &fleet,
        "ALTER TABLE users RENAME COLUMN name TO display_name",
    );
    q(&fleet, "ALTER TABLE users RENAME TO accounts");
    q(
        &fleet,
        "CREATE TABLE signups(account_id INTEGER, at TEXT DEFAULT 'now')",
    );
    q(
        &fleet,
        "CREATE TRIGGER log_signup AFTER INSERT ON accounts \
         BEGIN INSERT INTO signups(account_id) VALUES (new.id); END",
    );
    q(
        &fleet,
        "CREATE VIEW named AS SELECT id, display_name FROM accounts \
         WHERE display_name IS NOT NULL",
    );
    q(&fleet, "CREATE VIEW scratch_view AS SELECT 1");
    q(&fleet, "DROP VIEW scratch_view");

    // The introspection a migration tool reads, in both spellings.
    let columns: Vec<Value> = q(&fleet, "PRAGMA table_info(accounts)")
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[1].clone())
        .collect();
    assert_eq!(
        columns,
        vec![json!("id"), json!("display_name"), json!("email")]
    );
    assert_eq!(
        q(
            &fleet,
            "SELECT name FROM pragma_table_info('accounts') ORDER BY cid"
        ),
        json!([["id"], ["display_name"], ["email"]])
    );
    assert_eq!(q(&fleet, "PRAGMA index_list(accounts)"), json!([]));

    // The trigger fires on a write after the migration.
    q(
        &fleet,
        "INSERT INTO accounts(display_name, email) VALUES ('nova', 'n@x')",
    );
    assert_eq!(q(&fleet, "SELECT account_id FROM signups"), json!([[3]]));
    assert_eq!(
        q(&fleet, "SELECT display_name FROM named ORDER BY id"),
        json!([["aria"], ["octavia"], ["nova"]])
    );

    // The boundary. A RENAME onto a reserved name is refused AND rolled
    // back: the table is still there under its own name.
    forbidden(&fleet, DB, "ALTER TABLE accounts RENAME TO _dg_accounts");
    assert_eq!(q(&fleet, "SELECT count(*) FROM accounts"), json!([[3]]));
    assert_eq!(
        q(
            &fleet,
            "SELECT name FROM sqlite_master WHERE lower(name) LIKE '\\_dg\\_%' ESCAPE '\\'"
        ),
        json!([["_dg_replay"]])
    );
    forbidden(&fleet, DB, "ALTER TABLE _dg_replay ADD COLUMN x");
    forbidden(&fleet, DB, "CREATE VIEW _dg_view AS SELECT 1");
    forbidden(&fleet, DB, "PRAGMA journal_mode=DELETE");
    forbidden(&fleet, DB, "BEGIN");
    // A trigger may be written against anything, but its body is judged
    // when it fires, inside the caller's statement -- so one that reaches
    // for the replay table makes the write that fires it fail, not succeed
    // around the boundary.
    q(&fleet, "CREATE TABLE bait(x)");
    q(
        &fleet,
        "CREATE TRIGGER grab AFTER INSERT ON bait BEGIN DELETE FROM _dg_replay; END",
    );
    forbidden(&fleet, DB, "INSERT INTO bait VALUES (1)");
    q(&fleet, "DROP TRIGGER grab");
    q(&fleet, "DROP TABLE bait");

    // A read-only credential inspects; it does not migrate.
    let key = fleet.cli.ok(
        "read-only key",
        &[
            "key",
            "create",
            "--role",
            "database_read_only",
            "--database",
            DB,
            "reader",
        ],
    );
    let reader = fleet.cli.key_file.with_file_name("reader.key");
    std::fs::write(&reader, key["key"].as_str().unwrap()).unwrap();
    std::fs::set_permissions(&reader, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let reader = common::Cli {
        server: fleet.cli.server.clone(),
        key_file: reader,
    };
    reader.ok(
        "reader pragma",
        &["query", DB, "PRAGMA table_info(accounts)"],
    );
    for sql in [
        "ALTER TABLE accounts ADD COLUMN nope TEXT",
        "CREATE VIEW nope AS SELECT 1",
        "DROP TRIGGER log_signup",
    ] {
        reader.err(sql, &["query", DB, sql], "sql_forbidden");
    }

    // The operator views read a schema holding views and triggers.
    fleet.cli.ok("schema", &["schema", DB]);
    fleet.cli.ok("info", &["info", DB]);
    fleet.cli.ok("contents", &["contents"]);

    // Export → import: the backup path. The views and the trigger come
    // back, and the trigger still fires in the restored copy.
    let file = fleet.data_dir.with_file_name("app-export.sqlite");
    fleet
        .cli
        .ok("export", &["export", DB, "--out", file.to_str().unwrap()]);
    fleet.cli.ok(
        "import",
        &["import", "restored", "--from", file.to_str().unwrap()],
    );
    fleet.cli.ok("clone", &["clone", DB, "cloned"]);
    for copy in ["restored", "cloned"] {
        let rows = |sql: &str| fleet.cli.q(copy, copy, sql);
        assert_eq!(
            rows(
                "SELECT type, name FROM sqlite_master WHERE type IN ('view','trigger') ORDER BY name"
            ),
            json!([["trigger", "log_signup"], ["view", "named"]]),
            "{copy}"
        );
        rows("INSERT INTO accounts(display_name) VALUES ('vega')");
        assert_eq!(
            rows("SELECT count(*) FROM signups"),
            json!([[2]]),
            "{copy}: the trigger fires in the copy"
        );
        assert_eq!(rows("SELECT count(*) FROM named"), json!([[4]]), "{copy}");
    }
    let diff = fleet.cli.ok("diff", &["diff", DB, "cloned"]);
    assert!(diff.is_object(), "diff reads views and triggers: {diff}");
}
