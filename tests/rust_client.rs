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
    // The placement comes with the status: the client cannot work out
    // where the gateway is for itself, and a typed client that could not
    // see it would be a typed client with no answer to "where is this".
    let deployment = status
        .deployment
        .as_ref()
        .expect("the gateway reports its placement");
    assert!(deployment.listen.starts_with("127.0.0.1:"));
    assert!(!deployment.data_dir.is_empty());
    let placed = deployment
        .databases
        .get("rust_client_db")
        .expect("the created database is placed");
    assert!(
        placed.exists,
        "the created database has no file: {placed:?}"
    );
    assert!(placed.file.ends_with("rust_client_db.sqlite"));

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

#[tokio::test]
async fn a_request_is_given_up_on_as_what_it_is() {
    // Nothing is listening on port 1, so the request cannot arrive. A
    // read that is given up on costs a retry; a write that is given up
    // on leaves the outcome in doubt. The statements decide which one
    // this is — a query is a POST either way — so the same client says
    // the right thing about a SELECT and about an INSERT.
    let gateway = Gateway::new("http://127.0.0.1:1", "owner").expect("the origin is valid");
    let database = gateway.database("demo").expect("the name is valid");

    let read = database
        .execute("SELECT 1", vec![])
        .await
        .expect_err("the gateway is not there");
    assert_eq!(
        read.code, "transport_unavailable",
        "a read that cannot reach the gateway is an unavailable gateway: {read}"
    );
    assert!(
        !read.message.contains("may have committed"),
        "a read is reported as a write: {read}"
    );

    let write = database
        .execute("INSERT INTO t VALUES (1)", vec![])
        .await
        .expect_err("the gateway is not there");
    assert_eq!(
        write.code, "outcome_unknown",
        "a write that cannot reach the gateway leaves the outcome in doubt: {write}"
    );

    // A batch is a read only when every statement in it is one.
    let reads = database
        .batch(vec![Statement::new("SELECT 1"), Statement::new("SELECT 2")])
        .await
        .expect_err("the gateway is not there");
    assert_eq!(
        reads.code, "transport_unavailable",
        "a batch of reads is a read: {reads}"
    );

    let mixed = database
        .batch(vec![
            Statement::new("SELECT 1"),
            Statement::new("INSERT INTO t VALUES (1)"),
        ])
        .await
        .expect_err("the gateway is not there");
    assert_eq!(
        mixed.code, "outcome_unknown",
        "a batch that writes is a write: {mixed}"
    );
}

#[test]
fn the_client_and_the_server_agree_on_what_a_read_is() {
    // The client classifies by the statements it sends and the server
    // classifies by the statements it runs, with the same rule — so a
    // read does not sit in the write lane on one side and the read lane
    // on the other, and a read-only query is never reported as a write
    // that may have committed.
    let cases = [
        "SELECT 1",
        "select * from notes where id = ?1",
        "VALUES (1), (2)",
        "EXPLAIN QUERY PLAN SELECT 1",
        "WITH recent AS (SELECT id FROM notes) SELECT * FROM recent",
        "INSERT INTO notes VALUES (1)",
        "UPDATE notes SET body = 'x'",
        "DELETE FROM notes",
        "CREATE TABLE notes (id INTEGER)",
        "DROP TABLE notes",
        "WITH moved AS (SELECT id FROM notes) INSERT INTO notes SELECT id FROM moved",
        "ALTER TABLE notes ADD COLUMN body TEXT",
        "REPLACE INTO notes VALUES (1)",
        "VACUUM",
        "REINDEX notes",
        "  SELECT 1",
        "BEGIN",
        "PRAGMA journal_mode",
        // Literals and comments: a keyword inside one is text, not a
        // modifying statement. SQLite confirms each of these writes nothing
        // (rows returned, total_changes 0), so classifying them as writes
        // would report a read-only query as outcome_unknown.
        "SELECT 'DELETE'",
        "SELECT 'INSERT INTO notes VALUES (1)'",
        "SELECT 1 -- DELETE",
        "SELECT 1 /* INSERT INTO notes VALUES (1) */",
        "SELECT 1 -- DROP TABLE notes",
        "SELECT \"DELETE\" FROM notes",
        "SELECT `DROP` FROM notes",
        "SELECT [INSERT] FROM notes",
        "SELECT 'it''s a DELETE'",
        "WITH x AS (SELECT 'DROP TABLE notes') SELECT * FROM x",
        "WITH updated AS (SELECT 1) SELECT * FROM updated",
        // …and the keyword after a literal is still a keyword.
        "WITH m AS (SELECT 'none') DELETE FROM notes",
    ];
    for sql in cases {
        let server = dg::execution::is_read_only(&[dg::execution::Statement {
            sql: sql.to_string(),
            args: Vec::new(),
        }]);
        let client = dg_client::is_read_only(sql);
        assert_eq!(
            server, client,
            "the client and the server disagree about {sql:?}: the server says {server}, the client says {client}"
        );
    }
}
