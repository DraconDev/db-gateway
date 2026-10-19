//! Contract tests for the credential scanner (A1, docs/audit-2026-10-19.md).
//!
//! The scanner is a gate, so the behaviour worth protecting is: a
//! credential-shaped value in a tracked file is reported, a fake one is
//! not, and the report never contains the secret itself. Those three
//! properties are what make the gate usable -- a scanner that misses
//! real material is worse than none, and one that prints the value into
//! a CI log is a new leak.

use std::fs;
use std::path::Path;
use std::process::Command;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn scan(paths: &[&Path]) -> (bool, String) {
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/secret-scan.sh");
    for p in paths {
        cmd.arg(p);
    }
    let out = cmd.output().expect("run secret-scan");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn real_credential_shapes_are_reported() {
    let dir = tempdir();
    let aws = dir.path().join("aws.env");
    fs::write(
        &aws,
        "AWS_ACCESS_KEY_ID=[DRACON_SECRET:REDACTED]\n\
         aws_secret_access_key = EXAMPLEKEY\n",
    )
    .unwrap();
    let cf = dir.path().join("cf.env");
    fs::write(&cf, "CLOUDFLARE_API_TOKEN=kJ8hG5fD2sA9pL0kZ3xC7vB1nM4qW6eR8tY0uI2oP\n").unwrap();
    let ghp = dir.path().join("gh.env");
    fs::write(&ghp, "export GH_TOKEN=[DRACON_SECRET:REDACTED]\n").unwrap();
    let pem = dir.path().join("id.pem");
    fs::write(
        &pem,
        "[DRACON_SECRET:REDACTED]\n",
    )
    .unwrap();

    let (ok, out) = scan(&[&aws, &cf, &ghp, &pem]);
    assert!(!ok, "scanner passed a tree with real credential shapes:\n{out}");
    for kind in [
        "aws-access-key-id",
        "aws-secret-access-key",
        "generic-secret-assignment",
        "github-token",
        "private-key-block",
    ] {
        assert!(out.contains(kind), "scanner did not report {kind}:\n{out}");
    }
}

#[test]
fn the_report_never_contains_the_secret() {
    let dir = tempdir();
    let path = dir.path().join("leak.env");
    let secret = "EXAMPLEKEY";
    fs::write(&path, format!("aws_secret_access_key = {secret}\n")).unwrap();

    let (ok, out) = scan(&[&path]);
    assert!(!ok, "scanner passed a real secret:\n{out}");
    assert!(
        !out.contains(secret),
        "the scanner printed the secret value into its report:\n{out}"
    );
}

#[test]
fn known_fake_material_is_accepted() {
    let dir = tempdir();
    let path = dir.path().join("fixture.env");
    fs::write(
        &path,
        "# example value, not a real credential\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
         AWS_ACCESS_KEY_ID=[DRACON_SECRET:REDACTED]\n\
         export TOKEN=${GH_TOKEN}\n",
    )
    .unwrap();

    let (ok, out) = scan(&[&path]);
    assert!(ok, "scanner rejected a fixture with obvious placeholders:\n{out}");
}

#[test]
fn the_repository_tree_is_clean() {
    // The gate runs in CI over the whole tracked tree, so the tree
    // itself has to pass -- otherwise the gate is red for reasons that
    // have nothing to do with the change under test.
    let out = Command::new("bash")
        .arg("scripts/secret-scan.sh")
        .output()
        .expect("run secret-scan over the repo");
    assert!(
        out.status.success(),
        "secret-scan rejected the repository tree:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
