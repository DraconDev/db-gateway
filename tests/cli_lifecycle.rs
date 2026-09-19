//! Bounded and atomic lifecycle behavior through the real `dg` binary.
//!
//! These checks intentionally use argv/stdout/stderr/exit status only: no
//! direct HTTP client is involved.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;

#[test]
fn export_accepts_large_snapshots_and_publishes_without_clobbering() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "large_db"]);
    fleet.cli.q(
        "schema",
        "large_db",
        "CREATE TABLE payload (value BLOB NOT NULL)",
    );
    fleet.cli.q(
        "seed",
        "large_db",
        "INSERT INTO payload VALUES (zeroblob(400000))",
    );

    let dir = tempfile::tempdir().unwrap();
    let snapshot = dir.path().join("large.sqlite");
    let snapshot_arg = snapshot.to_str().unwrap();
    let (ok, stdout, stderr) = fleet.cli.run(&["export", "large_db", "--out", snapshot_arg]);
    assert!(ok, "large export failed: {stdout} {stderr}");
    assert!(
        std::fs::metadata(&snapshot).unwrap().len() > 256 * 1024,
        "fixture must exercise the lifecycle response bound"
    );

    let existing = dir.path().join("existing.sqlite");
    std::fs::write(&existing, b"keep this snapshot").unwrap();
    let existing_arg = existing.to_str().unwrap();
    let (ok, stdout, stderr) = fleet.cli.run(&["export", "large_db", "--out", existing_arg]);
    assert!(!ok, "existing export destination was overwritten: {stdout} {stderr}");
    assert!(
        stdout.contains("file_exists") || stderr.contains("file_exists"),
        "missing file_exists error: stdout={stdout} stderr={stderr}"
    );
    assert_eq!(std::fs::read(&existing).unwrap(), b"keep this snapshot");

    let imported = fleet.cli.ok(
        "import",
        &["import", "large_copy", "--from", snapshot_arg],
    );
    assert_eq!(imported["imported"], "large_copy");
    assert_eq!(
        fleet.cli.q("verify import", "large_copy", "SELECT length(value) FROM payload"),
        serde_json::json!([[400000]])
    );
}

#[test]
fn import_rejects_oversized_local_files_before_encoding_or_network() {
    let fleet = Fleet::boot();
    let dir = tempfile::tempdir().unwrap();
    let oversized = dir.path().join("oversized.sqlite");
    std::fs::write(&oversized, vec![0u8; 8 * 1024 * 1024 + 1]).unwrap();
    let oversized_arg = oversized.to_str().unwrap();
    let (ok, stdout, stderr) = fleet.cli.run(&[
        "import",
        "too_large",
        "--from",
        oversized_arg,
    ]);
    assert!(!ok, "oversized import unexpectedly succeeded: {stdout} {stderr}");
    assert!(
        stdout.contains("result_limit_exceeded") || stderr.contains("result_limit_exceeded"),
        "missing local size error: stdout={stdout} stderr={stderr}"
    );
    let (ok, stdout, stderr) = fleet.cli.run(&["query", "too_large", "SELECT 1"]);
    assert!(!ok, "oversized import published a database: {stdout} {stderr}");
    assert!(stdout.contains("database_not_found") || stderr.contains("database_not_found"));
}
