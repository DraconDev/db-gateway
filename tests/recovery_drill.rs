//! Local snapshot recovery drill (store-level, disposable directories).
//!
//! Run: `timeout 120 cargo test --locked --test recovery_drill`
//!
//! This is a manual-snapshot drill only, not OVH or continuous-backup
//! evidence (see docs/recovery-contract.md §6).

use dg::{execution, store::Store};

fn run(path: &std::path::Path, sql: &str) -> serde_json::Value {
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

fn rows(path: &std::path::Path, sql: &str) -> serde_json::Value {
    run(path, sql)["rows"].clone()
}

fn integrity_ok(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let bad: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(bad, 0);
}

#[test]
fn snapshot_export_restores_exact_rows_and_rejects_corrupt_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.bootstrap().unwrap();
    store.create("original").unwrap();
    let source = store.path("original").unwrap();
    for sql in [
        "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        "CREATE TABLE books (id INTEGER PRIMARY KEY, author_id INTEGER NOT NULL REFERENCES authors(id), title TEXT NOT NULL, price REAL, cover BLOB, note TEXT)",
        "INSERT INTO authors (id, name) VALUES (1, 'octavia'), (2, 'ursula')",
        "INSERT INTO books (id, author_id, title, price, cover, note) VALUES (1, 1, 'kindred', 9.5, X'0102FF', NULL), (2, 2, 'earthsea', 12.0, X'', 'classic')",
    ] {
        run(&source, sql);
    }
    let expected_authors = rows(&source, "SELECT id, name FROM authors ORDER BY id");
    let expected_books = rows(
        &source,
        "SELECT id, author_id, title, price, cover, note FROM books ORDER BY id",
    );

    // Export a snapshot and import it under a distinct name.
    let snapshot = store.export_snapshot("original").unwrap();
    integrity_ok(snapshot.path());
    store.import_from("restored", snapshot.path()).unwrap();
    let target = store.path("restored").unwrap();
    integrity_ok(&target);

    // Exact restored data, and the source is unchanged.
    assert_eq!(
        rows(&target, "SELECT id, name FROM authors ORDER BY id"),
        expected_authors
    );
    assert_eq!(
        rows(
            &target,
            "SELECT id, author_id, title, price, cover, note FROM books ORDER BY id"
        ),
        expected_books
    );
    assert_eq!(
        rows(&source, "SELECT id, name FROM authors ORDER BY id"),
        expected_authors
    );
    assert_eq!(
        rows(
            &source,
            "SELECT id, author_id, title, price, cover, note FROM books ORDER BY id"
        ),
        expected_books
    );

    // Restored copy is independent of the source.
    run(&target, "INSERT INTO authors (id, name) VALUES (3, 'nk')");
    assert_eq!(
        run(&source, "SELECT count(*) FROM authors")["rows"],
        serde_json::json!([[2]])
    );
    assert_eq!(
        run(&target, "SELECT count(*) FROM authors")["rows"],
        serde_json::json!([[3]])
    );

    // Corrupt input never publishes a target.
    let bad = dir.path().join("corrupt.sqlite");
    std::fs::write(&bad, b"not a sqlite file").unwrap();
    assert!(store.import_from("rejected", &bad).is_err());
    assert_eq!(
        store.path("rejected").unwrap_err().code,
        "database_not_found"
    );
}
