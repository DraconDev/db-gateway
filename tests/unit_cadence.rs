//! The two halves of the snapshot cadence must be the same number.
//!
//! The gateway's backup-health alarm (`/v1/backups`) and the daily
//! digest's staleness verdict are judged by the same number, but the
//! number is written twice — `--backup-cadence-secs` in
//! `deploy/dg.service`, `--cadence-secs` in
//! `deploy/dg-backup-digest.service` — and nothing compared them. A host
//! that rendered one half with a different value reported every database
//! overdue in one half and fresh in the other, behind a green deploy
//! gate, while the deployment guide claimed the two "cannot disagree".
//!
//! `scripts/check-unit-cadence.sh` is the check;
//! `scripts/validate-deploy.sh` runs it as part of the deploy gate.
//!
//! Run: `timeout 300 cargo test --locked --test unit_cadence`

use std::path::Path;
use std::process::Command;

/// Run the check against `root` and return (succeeded, stderr).
fn check(root: &Path) -> (bool, String) {
    let out = Command::new("bash")
        .arg("scripts/check-unit-cadence.sh")
        .arg(root)
        .output()
        .expect("run scripts/check-unit-cadence.sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A disposable deploy tree with the two units.
fn fixture(serve_cadence: Option<&str>, digest_cadence: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("deploy")).unwrap();
    let serve = match serve_cadence {
        Some(seconds) => format!(
            "[Service]\nExecStart=/usr/local/bin/dg serve --listen 127.0.0.1:7432 \
             --backup-cadence-secs {seconds}\n"
        ),
        None => {
            "[Service]\nExecStart=/usr/local/bin/dg serve --listen 127.0.0.1:7432\n".to_string()
        }
    };
    let digest = match digest_cadence {
        Some(seconds) => format!(
            "[Service]\nExecStart=/usr/local/bin/dg backup digest --cadence-secs {seconds}\n"
        ),
        None => "[Service]\nExecStart=/usr/local/bin/dg backup digest\n".to_string(),
    };
    std::fs::write(root.join("deploy/dg.service"), serve).unwrap();
    std::fs::write(root.join("deploy/dg-backup-digest.service"), digest).unwrap();
    dir
}

#[test]
fn the_two_halves_that_agree_pass() {
    let dir = fixture(Some("3600"), Some("3600"));
    let (ok, stderr) = check(dir.path());
    assert!(ok, "the check rejected two matching cadences: {stderr}");
}

#[test]
fn two_halves_that_disagree_are_refused() {
    let dir = fixture(Some("3600"), Some("900"));
    let (ok, stderr) = check(dir.path());
    assert!(!ok, "two different cadences must fail the check");
    assert!(
        stderr.contains("3600") && stderr.contains("900"),
        "the failure must name both numbers: {stderr}"
    );
}

#[test]
fn a_half_that_does_not_declare_its_cadence_is_refused() {
    let dir = fixture(None, Some("3600"));
    let (ok, stderr) = check(dir.path());
    assert!(!ok, "a unit without a cadence must fail the check");
    assert!(
        stderr.contains("--backup-cadence-secs"),
        "the failure must name the missing flag: {stderr}"
    );

    let dir = fixture(Some("3600"), None);
    let (ok, stderr) = check(dir.path());
    assert!(!ok, "a unit without a cadence must fail the check");
    assert!(
        stderr.contains("--cadence-secs"),
        "the failure must name the missing flag: {stderr}"
    );
}

#[test]
fn the_repository_units_declare_the_same_cadence() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (ok, stderr) = check(root);
    assert!(
        ok,
        "the shipped units disagree about the snapshot cadence: {stderr}"
    );
}
