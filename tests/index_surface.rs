//! Index authoring through the real `dg` binary.
//!
//! Run: `timeout 300 cargo test --locked --test index_surface`
//!
//! `CREATE INDEX` was refused by accident. SQLite calls the authorizer
//! more than once while building an index — `SQLITE_CREATE_INDEX` for the
//! index itself, then `SQLITE_REINDEX` for the pass that fills it — and
//! only the first was in the allow list, so the fail-closed default
//! denied the statement. The visible symptom was a gateway that could
//! `DROP INDEX` an index it refused to create.
//!
//! What this pins, all through the shipped binary:
//!
//! * a plain index and a unique index can be created, and `dg schema`
//!   reports both with the columns they cover and the right uniqueness;
//! * the unique one is enforced — a duplicate is rejected, not stored;
//! * `DROP INDEX` keeps working, and a bare `REINDEX` starts working —
//!   it was the same missing action, and it sweeps every index in the
//!   file, the gateway's own included;
//! * the `_dg_` guard survived the change: an index whose name claims to
//!   belong to the gateway is refused;
//! * a read-only credential cannot create, drop or reindex, and can still
//!   read.
//!
//! The first three would have failed before the `Reindex` action was
//! allowed. The last two would have passed either way, which is the point
//! of keeping them here: they are the boundary the change must not move,
//! and one of them moved anyway on the first attempt — a targeted
//! `REINDEX _dg_replay` was allowed because the callback reports
//! `sqlite_autoindex__dg_replay_1`, a name that does not begin with the
//! table's. That case is now asserted below.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

/// Create the table the index tests work against, and return the database.
fn table(fleet: &Fleet, name: &str) -> String {
    fleet.cli.ok(&format!("create {name}"), &["create", name]);
    fleet.cli.ok(
        "create table",
        &[
            "query",
            name,
            "CREATE TABLE instruments (id INTEGER PRIMARY KEY, isin TEXT, \
             notional INTEGER)",
        ],
    );
    name.to_string()
}

/// The one table entry `dg schema` returns.
fn schema_of(fleet: &Fleet, db: &str, table: &str) -> Value {
    let schema = fleet
        .cli
        .ok(&format!("schema {db} {table}"), &["schema", db, table]);
    schema["tables"][0].clone()
}

/// The names of the indexes `dg schema` reports for a table.
fn index_names(table: &Value) -> Vec<String> {
    table["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| index["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn an_index_can_be_created_seen_enforced_dropped_and_reindexed() {
    let fleet = Fleet::boot();
    let db = table(&fleet, "indexes_db");

    // A plain index, and a unique one. Both used to be 403 sql_forbidden;
    // `ok` asserts the statement was accepted and the schema assertions
    // below assert it took effect. `rows_affected` is deliberately NOT
    // asserted: for DDL it reports SQLite's `sqlite3_changes`, an
    // incidental detail of how one request is executed rather than part of
    // what "the index exists" means.
    for sql in [
        "CREATE INDEX instruments_by_isin ON instruments(isin)",
        "CREATE UNIQUE INDEX instruments_unique_notional ON instruments(notional)",
    ] {
        fleet.cli.ok(&format!("create {sql}"), &["query", &db, sql]);
    }

    // The schema view reports them, with their columns and uniqueness.
    let table = schema_of(&fleet, &db, "instruments");
    let names = index_names(&table);
    for expected in ["instruments_by_isin", "instruments_unique_notional"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} is missing from the schema: {names:?}"
        );
    }
    let indexes = table["indexes"].as_array().unwrap();
    let unique = indexes
        .iter()
        .find(|index| index["name"] == "instruments_unique_notional")
        .unwrap();
    assert_eq!(
        unique["unique"], true,
        "the unique index is not unique: {unique}"
    );
    assert_eq!(
        unique["columns"],
        serde_json::json!(["notional"]),
        "the unique index does not name its column: {unique}"
    );
    let plain = indexes
        .iter()
        .find(|index| index["name"] == "instruments_by_isin")
        .unwrap();
    assert_eq!(
        plain["unique"], false,
        "a plain index claims uniqueness: {plain}"
    );
    assert_eq!(
        plain["columns"],
        serde_json::json!(["isin"]),
        "the plain index does not name its column: {plain}"
    );

    // The unique index is enforced: the second insert is refused.
    fleet.cli.ok(
        "first insert",
        &[
            "query",
            &db,
            "INSERT INTO instruments(isin, notional) VALUES ('DE1', 100)",
        ],
    );
    // `invalid_sql` is the documented code for a statement SQLite refused;
    // a constraint violation is one. The row count below is the real
    // assertion, this one pins the code a client has to handle.
    fleet.cli.err(
        "duplicate insert",
        &[
            "query",
            &db,
            "INSERT INTO instruments(isin, notional) VALUES ('DE2', 100)",
        ],
        "invalid_sql",
    );
    let rows = fleet
        .cli
        .q("count", &db, "SELECT count(*) FROM instruments");
    assert_eq!(
        rows[0][0].as_i64().unwrap(),
        1,
        "the duplicate was stored despite the unique index"
    );

    // Dropping works, and the schema stops reporting the index — while
    // the table and its rows survive.
    fleet.cli.ok(
        "drop index",
        &["query", &db, "DROP INDEX instruments_by_isin"],
    );
    let after = index_names(&schema_of(&fleet, &db, "instruments"));
    assert!(
        !after.contains(&"instruments_by_isin".to_string()),
        "a dropped index is still reported: {after:?}"
    );
    assert!(
        after.contains(&"instruments_unique_notional".to_string()),
        "dropping one index took the other with it: {after:?}"
    );

    // A bare REINDEX is the same authorizer action, so it is part of the
    // same surface and has to keep working.
    fleet.cli.ok("reindex", &["query", &db, "REINDEX"]);
    let rows = fleet.cli.q(
        "count after reindex",
        &db,
        "SELECT count(*) FROM instruments",
    );
    assert_eq!(rows[0][0].as_i64().unwrap(), 1, "REINDEX lost the rows");
}

#[test]
fn the_gateway_guard_still_refuses_its_own_objects() {
    let fleet = Fleet::boot();
    let db = table(&fleet, "guard_db");

    // A `_dg_` prefix is the gateway's own namespace. Allowing REINDEX must
    // not have opened a door into it: an index can be NAMED anything, and a
    // name is the only handle the authorizer has.
    fleet.cli.err(
        "protected index name",
        &[
            "query",
            &db,
            "CREATE INDEX _dg_instruments ON instruments(isin)",
        ],
        "sql_forbidden",
    );
    // The table behind a protected index name is guarded the same way.
    fleet.cli.err(
        "protected table for an index",
        &[
            "query",
            &db,
            "CREATE INDEX instruments_dg ON _dg_replay(isin)",
        ],
        "sql_forbidden",
    );
    // Nothing was created by either attempt.
    let names = index_names(&schema_of(&fleet, &db, "instruments"));
    assert!(
        names.is_empty(),
        "a refused index was created anyway: {names:?}"
    );

    // A targeted REINDEX reaches the gateway's own table indirectly: SQLite
    // rebuilds `_dg_replay`'s implicit index and reports it as
    // `sqlite_autoindex__dg_replay_1`, which does not begin with `_dg_`.
    // Naming the table, naming that index, and qualifying either with the
    // schema all have to be refused — a bare `REINDEX` (asserted in the
    // test above) stays allowed, which is why this cannot be enforced by
    // the authorizer alone.
    for sql in [
        "REINDEX _dg_replay",
        "REINDEX \"sqlite_autoindex__dg_replay_1\"",
        "REINDEX main._dg_replay",
        "REINDEX _DG_REPLAY",
        // A comment is whitespace to SQLite, so a screen that reads the
        // statement text has to read it the way SQLite does -- including
        // the case that first got through here, a `--` inside a block
        // comment, which a naive line-comment scanner swallows along with
        // the rest of the statement.
        "REINDEX /*c*/ _dg_replay",
        "REINDEX main . _dg_replay",
        "REINDEX/*--*/_dg_replay",
        "REINDEX /* -- */ _dg_replay",
        "REINDEX main./*--*/_dg_replay",
        "REINDEX/* /* */_dg_replay",
        // `EXPLAIN` compiles the statement without running it and returns
        // bytecode naming the index it would have rebuilt.
        "EXPLAIN REINDEX _dg_replay",
        "EXPLAIN QUERY PLAN REINDEX _dg_replay",
    ] {
        fleet.cli.err(
            &format!("targeted reindex of {sql}"),
            &["query", &db, sql],
            "sql_forbidden",
        );
    }
    // And the screen does not over-reach: a user's own table and index are
    // both legitimate REINDEX targets.
    fleet.cli.ok(
        "reindex the user's index",
        &["query", &db, "CREATE INDEX t_by_a ON instruments(isin)"],
    );
    fleet
        .cli
        .ok("reindex an index", &["query", &db, "REINDEX t_by_a"]);
    fleet
        .cli
        .ok("reindex a table", &["query", &db, "REINDEX instruments"]);
}

#[test]
fn a_read_only_credential_can_not_touch_indexes() {
    let fleet = Fleet::boot();
    let db = table(&fleet, "readonly_db");
    fleet.cli.ok(
        "owner index",
        &["query", &db, "CREATE INDEX owner_idx ON instruments(isin)"],
    );

    let issued = fleet.cli.ok(
        "issue read-only key",
        &[
            "key",
            "create",
            "reader",
            "--role",
            "database_read_only",
            "--database",
            &db,
        ],
    );
    let key = issued["key"].as_str().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("reader.key");
    std::fs::write(&key_file, &key).unwrap();
    // The CLI refuses a credential file other users could read, and
    // `fs::write` creates one 0644.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let reader = common::Cli {
        server: fleet.cli.server.clone(),
        key_file,
    };

    for sql in [
        "CREATE INDEX reader_idx ON instruments(isin)",
        "DROP INDEX owner_idx",
        "REINDEX",
    ] {
        reader.err(
            &format!("read-only {sql}"),
            &["query", &db, sql],
            "sql_forbidden",
        );
    }
    // The index is still there, and reading still works.
    let rows = reader.q("read", &db, "SELECT count(*) FROM instruments");
    assert_eq!(rows[0][0].as_i64().unwrap(), 0);
    let names = index_names(&schema_of(&fleet, &db, "instruments"));
    assert_eq!(
        names,
        vec!["owner_idx".to_string()],
        "a read-only credential changed the schema: {names:?}"
    );
}
