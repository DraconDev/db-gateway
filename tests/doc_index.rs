//! The documentation index must register every file under `docs/`.
//!
//! `docs/README.md` opens with a promise: "Every file is one of:
//! Canonical, Historical evidence, Superseded". A file the register does not
//! name is a file nobody can find from the index, and the promise is false.
//!
//! The promise was kept once by hand (2026-10-19) and drifted again: the
//! evidence captured afterwards was never added, so the index lost the
//! Turso captures and the release receipts. A one-off listing does not
//! hold; `scripts/check-doc-index.py` is the check that holds, and
//! `scripts/validate-deploy.sh` runs it as part of the deploy gate.
//!
//! Run: `timeout 300 cargo test --locked --test doc_index`

use std::path::Path;
use std::process::Command;

/// Run the check against `root` and return (succeeded, stderr).
fn check(root: &Path) -> (bool, String) {
    let out = Command::new("python3")
        .arg("scripts/check-doc-index.py")
        .arg(root)
        .output()
        .expect("run scripts/check-doc-index.py");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A disposable docs tree with an index that names the given files.
fn fixture(registered: &[&str], unregistered: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs/evidence")).unwrap();
    std::fs::create_dir_all(root.join("docs/verification")).unwrap();
    let mut index = String::from("# Documentation index\n\n## Canonical\n\n");
    for name in registered {
        index.push_str(&format!("- `{name}`\n"));
    }
    std::fs::write(root.join("docs/README.md"), index).unwrap();
    for name in registered.iter().chain(unregistered.iter()) {
        let path = root.join("docs").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "evidence\n").unwrap();
    }
    dir
}

#[test]
fn an_index_that_names_every_file_passes() {
    let dir = fixture(
        &["deployment.md", "evidence/run.txt"],
        &[],
    );
    let (ok, stderr) = check(dir.path());
    assert!(ok, "the check rejected a complete index: {stderr}");
}

#[test]
fn a_file_the_index_does_not_name_is_refused() {
    let dir = fixture(&["deployment.md"], &["evidence/unlisted.txt"]);
    let (ok, stderr) = check(dir.path());
    assert!(!ok, "an unlisted file must fail the check");
    assert!(
        stderr.contains("evidence/unlisted.txt"),
        "the failure must name the file: {stderr}"
    );
}

#[test]
fn a_second_readme_needs_its_path_not_just_its_name() {
    // `docs/README.md` is the index itself and `docs/verification/README.md`
    // is another file. A check that matched the bare name would let the
    // second one pass on the index's own mention, which is exactly the
    // hole that let the receipts go unregistered.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs/verification")).unwrap();
    std::fs::write(root.join("docs/README.md"), "# Index\n").unwrap();
    std::fs::write(root.join("docs/verification/README.md"), "receipts\n").unwrap();
    let (ok, stderr) = check(root);
    assert!(!ok, "a second README must be named by its path");
    assert!(
        stderr.contains("verification/README.md"),
        "the failure must name the path: {stderr}"
    );
}

#[test]
fn the_repository_index_registers_every_file() {
    // The real tree: the gate that runs in CI is this same check, so a
    // file added under docs/ without being registered fails the suite.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (ok, stderr) = check(root);
    assert!(ok, "the docs index does not register every file: {stderr}");
}
