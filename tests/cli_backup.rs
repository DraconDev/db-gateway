//! The scheduled backup digest must not disturb the live gateway.
//!
//! `dg backup digest` runs as a SECOND process on the host where
//! `dg.service` owns the data directory — `deploy/dg-backup-digest.service`
//! runs it as a daily oneshot on the same machine. The owner keeps
//! chunked-export and lifecycle staging directories in that directory
//! while a transfer is in flight.
//!
//! The reader path therefore performs none of the owner's startup work.
//! Before this was true, the orphan sweep in `Store::open` deleted those
//! directories, so a chunked export that overlapped the daily digest lost
//! its snapshot and failed with an unexplained 500, and a chunked import
//! lost its staging file and failed with an unexplained 400.
//!
//! Run: `timeout 120 cargo test --locked --test cli_backup`

use std::process::Command;

/// A directory the live gateway is using, named exactly as it names it.
fn in_flight_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    let dir = data_dir.join(".transfer-inflight");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("snapshot.sqlite"), b"in flight").unwrap();
    dir
}

#[test]
fn the_digest_leaves_the_owners_in_flight_directories_alone() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let live = in_flight_dir(&data_dir);

    let out = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["backup", "digest", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        live.exists(),
        "the digest deleted a directory the owner was still using: {out:?}"
    );
    assert_eq!(
        std::fs::read(live.join("snapshot.sqlite")).unwrap(),
        b"in flight",
        "the digest disturbed the contents of the owner's directory"
    );
}

#[test]
fn the_digest_reports_on_a_registry_it_did_not_create() {
    // A reader must not write the owner's schema: the owner creates it.
    // The digest still has to answer, so an uninitialised registry is
    // reported as "no databases" rather than as a failure.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["backup", "digest", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the digest failed on an empty data directory: {out:?}"
    );
    // The report, not the wording of a section header: a reader must be
    // told how many databases it found, and on a registry it did not
    // create that number is zero.
    assert!(
        stdout.contains("0 database"),
        "the digest did not report an empty registry: {stdout}"
    );
}
