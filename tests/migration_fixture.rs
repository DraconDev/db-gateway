//! Migration-fixture rehearsal: synthetic *external* SQLite data through the
//! existing import API.
//!
//! Run: `timeout 120 cargo test --locked --test migration_fixture`
//!
//! This proves the gateway import path handles representative external
//! schemas (tables, indexes, foreign keys, all value types) and rejects an
//! incompatible fixture without publication. It is NOT evidence of Turso
//! compatibility — see docs/migration-inventory.md.

use dg::{execution, store::Store};

fn query(path: &std::path::Path, sql: &str) -> serde_json::Value {
    execution::execute(
        path,
        &[execution::Statement {
            sql: sql.into(),
            args: vec![],
        }],
        false,
        false,
        "owner",
        None,
    )
    .unwrap()
}

fn build_external(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE authors (
           id INTEGER PRIMARY KEY,
           name TEXT NOT NULL,
           rating REAL NOT NULL,
           bio TEXT
         );
         CREATE TABLE books (
           id INTEGER PRIMARY KEY,
           author_id INTEGER NOT NULL REFERENCES authors(id),
           title TEXT NOT NULL,
           price REAL NOT NULL,
           stock INTEGER NOT NULL,
           cover BLOB,
           meta TEXT NOT NULL,
           is_active INTEGER NOT NULL
         );
         CREATE INDEX idx_books_author ON books(author_id);
         CREATE INDEX idx_books_title ON books(title);
         INSERT INTO authors VALUES (1, 'Ada', 4.75, NULL);
         INSERT INTO authors VALUES (2, 'Grace', 4.9, 'pioneer');
         INSERT INTO books VALUES (1, 1, 'Notes', 19.99, 7, X'DEADBEEF', '{\"pages\": 120}', 1);
         INSERT INTO books VALUES (2, 2, 'Compiler', 0.0, 0, NULL, '{\"pages\": 0}', 0);",
    )
    .unwrap();
    drop(conn);
}

#[test]
fn synthetic_external_sqlite_imports_with_exact_data() {
    let dir = tempfile::tempdir().unwrap();
    let external = dir.path().join("external.sqlite");
    build_external(&external);

    let store = Store::open(dir.path()).unwrap();
    store.bootstrap().unwrap();
    store.import_from("migrated", &external).unwrap();

    let published = store.path("migrated").unwrap();
    // Exact restored data across every supported value type, join included.
    let rows = query(
        &published,
        "SELECT b.id, a.name, b.title, b.price, b.stock, b.meta, b.is_active \
         FROM books b JOIN authors a ON a.id = b.author_id ORDER BY b.id",
    );
    assert_eq!(
        rows["rows"],
        serde_json::json!([
            [1, "Ada", "Notes", 19.99, 7, "{\"pages\": 120}", 1],
            [2, "Grace", "Compiler", 0.0, 0, "{\"pages\": 0}", 0],
        ])
    );
    // NULL and BLOB round-trip.
    let nulls = query(&published, "SELECT bio FROM authors WHERE id = 1");
    assert_eq!(nulls["rows"], serde_json::json!([[null]]));
    let blob = query(&published, "SELECT hex(cover) FROM books WHERE id = 1");
    assert_eq!(blob["rows"], serde_json::json!([["DEADBEEF"]]));
    // Indexes survived the import.
    let indexes = query(
        &published,
        "SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name",
    );
    let names: Vec<_> = indexes["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"idx_books_author".to_string()), "{names:?}");
    assert!(names.contains(&"idx_books_title".to_string()), "{names:?}");
}

#[test]
fn incompatible_fixture_rejected_without_publication() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.sqlite");
    {
        let conn = rusqlite::Connection::open(&bad).unwrap();
        conn.execute_batch(
            "CREATE TABLE ok (id INTEGER PRIMARY KEY);
             CREATE TABLE _dg_shadow (id INTEGER);",
        )
        .unwrap();
        drop(conn);
    }
    let store = Store::open(dir.path()).unwrap();
    store.bootstrap().unwrap();
    let error = store.import_from("smuggled", &bad).unwrap_err();
    assert_eq!(error.code, "invalid_request");
    // Nothing published: no registry row, no file in the data dir.
    assert_eq!(
        store.path("smuggled").unwrap_err().code,
        "database_not_found"
    );
    assert!(!dir.path().join("smuggled.sqlite").exists());
}
