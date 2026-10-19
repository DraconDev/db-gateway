//! Contract tests for the credential scanner (A1, docs/audit-2026-10-19.md).
//!
//! The scanner is a gate, so the behaviour worth protecting is: a
//! credential-shaped value in a tracked file is reported, a fake one is
//! not, and the report never contains the secret itself. Those three
//! properties are what make the gate usable -- a scanner that misses
//! real material is worse than none, and one that prints the value into
//! a CI log is a new leak.
//!
//! The fixture values below are assembled at run time from split
//! fragments on purpose. Written out literally, this file would contain
//! credential-shaped strings -- which the scanner would (correctly)
//! report against the repository tree, failing the "tree is clean" test
//! below. Assembling them keeps the fixtures byte-for-byte realistic
//! while leaving no literal credential shape anywhere in the tracked
//! tree, and it means the gate needs no per-file exemption for its own
//! test.

use std::fs;
use std::path::Path;
use std::process::Command;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// Join fragments into one value at run time (see the module comment).
fn joined(parts: &[&str]) -> String {
    parts.concat()
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
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn real_credential_shapes_are_reported() {
    let dir = tempdir();

    let key_id = joined(&["AKIA", "Q7Z4XZ2MPL9RB3TW"]);
    let secret = joined(&["hK7mQ2vN9pLxR4tZ8cW3yB6dF1gJ5sA0eU2iO7n", "M"]);
    let aws = dir.path().join("aws.env");
    fs::write(
        &aws,
        format!("AWS_ACCESS_KEY_ID={key_id}\naws_secret_access_key = {secret}\n"),
    )
    .unwrap();

    let cf_token = joined(&["kJ8hG5fD2sA9pL0kZ3xC7vB1nM4qW6eR8tY0uI2o", "P"]);
    let cf = dir.path().join("cf.env");
    fs::write(&cf, format!("CLOUDFLARE_API_TOKEN={cf_token}\n")).unwrap();

    let ghp = joined(&["ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"]);
    let gh = dir.path().join("gh.env");
    fs::write(&gh, format!("export GH_TOKEN={ghp}\n")).unwrap();

    let pem = dir.path().join("id.pem");
    fs::write(
        &pem,
        format!(
            "{}\nMIIEowIBAAKCAQEA\n{}\n",
            joined(&["-----BEGIN ", "RSA PRIVATE KEY-----"]),
            joined(&["-----END ", "RSA PRIVATE KEY-----"]),
        ),
    )
    .unwrap();

    let (ok, out) = scan(&[&aws, &cf, &gh, &pem]);
    assert!(
        !ok,
        "scanner passed a tree with real credential shapes:\n{out}"
    );
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
    let secret = joined(&["hK7mQ2vN9pLxR4tZ8cW3yB6dF1gJ5sA0eU2iO7n", "M"]);
    fs::write(&path, format!("aws_secret_access_key = {secret}\n")).unwrap();

    let (ok, out) = scan(&[&path]);
    assert!(!ok, "scanner passed a real secret:\n{out}");
    assert!(
        !out.contains(&secret),
        "the scanner printed the secret value into its report:\n{out}"
    );
}

#[test]
fn known_fake_material_is_accepted() {
    let dir = tempdir();
    let path = dir.path().join("fixture.env");
    // The AWS documentation's own example key, a value labelled as not
    // real, and a shell expansion: none of these is a leak, and a gate
    // that flags them gets switched off.
    fs::write(
        &path,
        format!(
            "# example value, not a real credential\n\
             aws_secret_access_key = {}\n\
             AWS_ACCESS_KEY_ID={}\n\
             export TOKEN=${{GH_TOKEN}}\n",
            joined(&["wJalrXUtnFEMI/K7MDENG/bPxRfiCY", "EXAMPLEKEY"]),
            joined(&["AKIAIOSFODNN7", "EXAMPLE"]),
        ),
    )
    .unwrap();

    let (ok, out) = scan(&[&path]);
    assert!(
        ok,
        "scanner rejected a fixture with obvious placeholders:\n{out}"
    );
}

#[test]
fn the_repository_tree_is_clean() {
    // The gate runs in CI over the whole tracked tree, so the tree
    // itself has to pass -- otherwise the gate is red for reasons that
    // have nothing to do with the change under test. This also proves
    // the fixtures above leave no credential shape in the repository.
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
