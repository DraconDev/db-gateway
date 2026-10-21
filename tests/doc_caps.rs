//! The live documents must state the gateway's memory cap the way the
//! unit sets it.
//!
//! The unit file `deploy/dg.service` is the fact the documents have to
//! agree with: a document that states a different ceiling for the
//! gateway contradicts the file that is actually installed, and the
//! operator reads the document. One pass corrected the cap in three
//! documents and left a fourth claiming 96 MiB twenty lines below the
//! correction — the same file contradicting itself.
//!
//! `scripts/check-doc-caps.py` is that check; `scripts/validate-deploy.sh`
//! runs it as part of the deploy gate.
//!
//! Run: `timeout 300 cargo test --locked --test doc_caps`

use std::path::Path;
use std::process::Command;

/// Run the check against `root` and return (succeeded, stderr).
fn check(root: &Path) -> (bool, String) {
    let out = Command::new("python3")
        .arg("scripts/check-doc-caps.py")
        .arg(root)
        .output()
        .expect("run scripts/check-doc-caps.py");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A disposable tree with the unit and one document.
fn fixture(unit_cap: &str, document: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("deploy")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("deploy/dg.service"),
        format!("[Service]\nMemoryMax={unit_cap}\n"),
    )
    .unwrap();
    std::fs::write(root.join("docs/note.md"), document).unwrap();
    dir
}

#[test]
fn a_document_that_states_the_units_own_cap_passes() {
    let dir = fixture(
        "64M",
        "The unit file `deploy/dg.service` sets `MemoryMax=64M`, and the host\n\
         enforces the same 64 MiB (67108864 bytes).\n",
    );
    let (ok, stderr) = check(dir.path());
    assert!(ok, "the check rejected an agreeing document: {stderr}");
}

#[test]
fn a_document_that_states_a_different_cap_is_refused() {
    let dir = fixture(
        "64M",
        "The release script's ceiling is the one used in\n\
         `deploy/dg.service` (96 MiB), with the supervised run as the\n\
         on-target confirmation.\n",
    );
    let (ok, stderr) = check(dir.path());
    assert!(
        !ok,
        "a document claiming 96 MiB for a 64M unit must be refused"
    );
    assert!(
        stderr.contains("96 MiB"),
        "the refusal must name the offending figure: {stderr}"
    );
    assert!(
        stderr.contains("docs/note.md"),
        "the refusal must name the offending document: {stderr}"
    );
}

#[test]
fn a_document_quoting_a_different_quantity_is_not_refused() {
    // The page-cache term and the per-connection cache ceiling are not
    // the gateway's cap: a document may quote them next to the unit's
    // name without contradicting it.
    let dir = fixture(
        "64M",
        "The gateway ships `MemoryMax=64M` (`deploy/dg.service`); the page-cache\n\
         term is ~24 MiB, not ~10 MiB — two independent admission lanes.\n",
    );
    let (ok, stderr) = check(dir.path());
    assert!(
        ok,
        "the check rejected a document quoting another quantity: {stderr}"
    );
}

#[test]
fn the_repository_is_consistent() {
    let (ok, stderr) = check(Path::new("."));
    assert!(
        ok,
        "the live documents disagree with the unit's memory cap: {stderr}"
    );
}
