//! Integration test for the row-count + PRAGMA integrity_check
//! comparison logic that `scripts/litestream-restore.sh` runs after every
//! `litestream restore`.
//!
//! The test does not touch the live host or the live bucket. It builds a
//! synthetic SQLite database, mirrors it (by copy, which is what a
//! successful `litestream restore` would also produce), and then runs the
//! same row-count + integrity_check comparison that the live script uses.
//! The point is to pin the comparison logic so a future change to the
//! shell script can be checked against this unit test's expectations.
//!
//! This is the test for the goal's verification contract clause "row
//! counts plus PRAGMA integrity_check match the live sources exactly" —
//! verified here on a synthetic pair, and the live script applies the
//! same logic to the real restored files on micro2.

use rusqlite::Connection;
use std::path::Path;

#[test]
fn integrity_check_passes_on_a_fresh_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); \
         INSERT INTO t(v) VALUES ('one'), ('two'), ('three');",
    )
    .unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check;", [], |r| r.get(0))
        .expect("integrity_check query");
    assert_eq!(integrity, "ok", "a freshly-created db must report 'ok'");
}

#[test]
fn row_counts_match_when_the_copies_are_identical() {
    let (source, restored, _dir) = build_mirror_pair(2, 5);
    let (src, cmp) = count_per_table(&source, &restored);
    assert_eq!(src, cmp, "row counts must match between identical copies");
}

#[test]
fn row_counts_diverge_when_a_row_is_missing() {
    let (source, mut restored, _dir) = build_mirror_pair(2, 4);
    // The "restored" copy missed one row; the comparison must catch it.
    delete_row(&mut restored, "t0", 1).unwrap();
    let (src, cmp) = count_per_table(&source, &restored);
    assert_ne!(src, cmp, "a missing row must be detected by the comparison");
}

#[test]
fn integrity_check_fails_on_a_corrupted_db() {
    let (_source, mut restored, _dir) = build_mirror_pair(2, 3);
    // Truncate the file: simulates a torn write or a corrupted copy.
    std::fs::write(&restored, b"not a sqlite database").unwrap();
    // A non-sqlite file must either fail to open, or open and report a
    // non-'ok' integrity_check. Either outcome satisfies the contract:
    // a corrupted restore is never silently accepted.
    let integrity_outcome = match Connection::open(&restored) {
        Ok(c) => match c.query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0)) {
            Ok(s) => Ok(s),
            Err(e) => Err(format!("query: {e}")),
        },
        Err(e) => Err(format!("open: {e}")),
    };
    let integrity = match integrity_outcome {
        Ok(s) => s,
        Err(_) => "not_ok".to_string(),
    };
    assert_ne!(
        integrity, "ok",
        "a truncated or non-sqlite file must not report 'ok' (got {integrity:?})"
    );
}

fn build_mirror_pair(tables: usize, rows_per_table: usize) -> (std::path::PathBuf, std::path::PathBuf, tempfile::TempDir) {
    // Return the TempDir alongside the paths so the caller keeps it alive;
    // the test fixture would otherwise be deleted when the helper returns
    // and every subsequent read would race a missing file.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.sqlite");
    let restored = dir.path().join("restored.sqlite");
    let conn = Connection::open(&source).unwrap();
    for t in 0..tables {
        let name = format!("t{t}");
        conn.execute_batch(&format!(
            "CREATE TABLE {name} (id INTEGER PRIMARY KEY, v TEXT);"
        ))
        .unwrap();
        for r in 0..rows_per_table {
            conn.execute(
                &format!("INSERT INTO {name}(v) VALUES ('v{r}')"),
                [],
            )
            .unwrap();
        }
    }
    drop(conn);
    std::fs::copy(&source, &restored).unwrap();
    (source, restored, dir)
}

fn delete_row(
    path: &Path,
    table: &str,
    row_id: i64,
) -> rusqlite::Result<()> {
    let conn = Connection::open(path)?;
    conn.execute(&format!("DELETE FROM {table} WHERE id = ?1"), [row_id])?;
    Ok(())
}

fn count_per_table(a: &Path, b: &Path) -> (Vec<(String, i64)>, Vec<(String, i64)>) {
    let ca = Connection::open(a).unwrap();
    let cb = Connection::open(b).unwrap();
    let mut out_a = vec![];
    let mut out_b = vec![];
    for table in tables(&ca) {
        let n: i64 = ca
            .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))
            .expect("count on source");
        out_a.push((table.clone(), n));
    }
    for table in tables(&cb) {
        let n: i64 = cb
            .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))
            .expect("count on restored");
        out_b.push((table.clone(), n));
    }
    out_a.sort();
    out_b.sort();
    (out_a, out_b)
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}
