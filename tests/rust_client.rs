//! Integration proof for the Rust application client (`crates/dg-client`,
//! audit A6): the typed crate drives a real `dg serve` through the exact
//! workflow a Rust production writer (e.g. `api_core::db::SharedDb`
//! callers) will use — scoped-key auth, typed values, atomic batch,
//! keyed replay, and out-of-scope refusal. Before this crate the only
//! application client was TypeScript.

mod common;

use dg_client::{Gateway, Statement, Value};

#[tokio::test]
async fn rust_client_drives_a_live_gateway_end_to_end() {
    let fleet = common::Fleet::boot();
    let owner = Gateway::from_key_file(&fleet.cli.server, &fleet.cli.key_file)
        .expect("owner gateway from key file");

    // Lifecycle + discovery through the typed owner surface.
    owner.create_database("rust_client_db").await.unwrap();
    assert!(
        owner
            .list_databases()
            .await
            .unwrap()
            .contains(&"rust_client_db".to_string())
    );
    let status = owner.status().await.unwrap();
    assert!(!status.version.is_empty());

    // Typed DDL + writes with every value kind.
    let db = owner.database("rust_client_db").unwrap();
    db.execute(
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, weight REAL, flag INTEGER, payload BLOB)",
        vec![],
    )
    .await
    .unwrap();
    let inserted = db
        .execute(
            "INSERT INTO notes VALUES (?1, ?2, ?3, ?4, ?5)",
            vec![
                Value::Integer(1),
                Value::Text("héllo".into()),
                Value::Real(1.5),
                Value::Bool(true),
                Value::Blob(vec![0, 1, 255]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(inserted.rows_affected, 1);

    // Keyed write replayed with the same key must not double-apply.
    let first = db
        .execute_with_key(
            "INSERT INTO notes (id, body) VALUES (2, 'keyed')",
            vec![],
            "rust-client-test-key-1",
        )
        .await
        .unwrap();
    let replay = db
        .execute_with_key(
            "INSERT INTO notes (id, body) VALUES (2, 'keyed')",
            vec![],
            "rust-client-test-key-1",
        )
        .await
        .unwrap();
    assert_eq!(first, replay);
    let count = db
        .execute("SELECT count(*) FROM notes WHERE id = 2", vec![])
        .await
        .unwrap();
    assert_eq!(count.rows, vec![vec![Value::Integer(1)]]);

    // Atomic batch + exact typed read-back.
    db.batch(vec![
        Statement::new("INSERT INTO notes (id, body) VALUES (3, 'b1')"),
        Statement::new("INSERT INTO notes (id, body) VALUES (4, 'b2')"),
    ])
    .await
    .unwrap();
    let rows = db
        .execute(
            "SELECT id, body, weight, payload FROM notes WHERE id = 1",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![
            Value::Integer(1),
            Value::Text("héllo".into()),
            Value::Real(1.5),
            Value::Blob(vec![0, 1, 255]),
        ]]
    );
    let info = db.info().await.unwrap();
    assert_eq!(
        info.tables.iter().find(|t| t.name == "notes").unwrap().rows,
        4
    );

    // A scoped read-only key for a DIFFERENT database is refused here,
    // and a read-only key cannot write to its own database.
    let scoped = fleet.cli.ok(
        "scoped key",
        &[
            "key",
            "create",
            "rc_reader",
            "--role",
            "database_read_only",
            "--database",
            "rust_client_db",
        ],
    );
    let reader = Gateway::new(&fleet.cli.server, scoped["key"].as_str().unwrap()).unwrap();
    let read = reader
        .database("rust_client_db")
        .unwrap()
        .execute("SELECT count(*) FROM notes", vec![])
        .await
        .unwrap();
    assert_eq!(read.rows, vec![vec![Value::Integer(4)]]);
    let denied = reader
        .database("rust_client_db")
        .unwrap()
        .execute("INSERT INTO notes (id) VALUES (9)", vec![])
        .await
        .expect_err("read-only key must not write");
    assert_eq!(denied.status, Some(403));

    owner.create_database("keys_elsewhere").await.unwrap();
    let other = fleet.cli.ok(
        "other-db key",
        &[
            "key",
            "create",
            "rc_other",
            "--role",
            "database_read_write",
            "--database",
            "keys_elsewhere",
        ],
    );
    let outsider = Gateway::new(&fleet.cli.server, other["key"].as_str().unwrap()).unwrap();
    let refused = outsider
        .database("rust_client_db")
        .unwrap()
        .execute("SELECT 1", vec![])
        .await
        .expect_err("out-of-scope key must be refused");
    assert_eq!(refused.status, Some(403));
}
