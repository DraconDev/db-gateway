//! Contract tests for the publication gate (`scripts/public-readiness.sh`).
//!
//! The gate exists because private-ness was doing the work of a control
//! that nothing enforced: two real leaks were sitting in the tracked tree
//! when publication was first attempted, and neither of them carried a
//! credential-shaped string, so the existing credential scanner could not
//! see either one. See `docs/publication-readiness.md`.
//!
//! What is worth pinning is not the report layout but the verdict:
//!
//!   * a tracked session transcript fails the gate,
//!   * a tracked private-key file fails the gate,
//!   * a tracked build output directory fails the gate,
//!   * a tracked oversized blob fails the gate,
//!   * a tree carrying none of those passes,
//!   * and the report never prints the contents of a file it refuses.
//!
//! The last one matters most. The gate names paths on purpose and never
//! values, on the same reasoning as `scripts/secret-scan.sh`: a checker
//! whose output can be pasted into an issue is itself a leak.
//!
//! Every fixture here is built in a throwaway git repository, because the
//! gate reads `git ls-files` and therefore has to be pointed at a real
//! index. The `--tree` mode is used throughout: the history checks walk
//! every commit in the target repo and belong to a pre-publication run,
//! not to a unit test.
//!
//! The credential-shaped fixture values are assembled from split fragments
//! at run time, exactly as `tests/secret_scan.rs` does, so this file
//! contains no literal credential shape and the repository's own tree scan
//! stays green.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Join fragments into one value at run time (see the module comment).
fn joined(parts: &[&str]) -> String {
    parts.concat()
}

/// The gate under test, at its real path in this repository.
fn gate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/public-readiness.sh")
}

/// A git repository with `files` tracked and committed, returned as a
/// `TempDir` so it is removed when the test ends.
fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();

    for (rel, body) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture dir");
        }
        fs::write(&path, body).expect("fixture file");
    }

    // A sparse config rather than the host's: the fixture must not depend
    // on the ambient git config, and `-c user.*` keeps the commit possible
    // on a box that has no identity set. GIT_CONFIG_GLOBAL/SYSTEM point at
    // /dev/null so a developer's global ignore file cannot decide what
    // lands in the fixture index -- a global `*.env` rule would otherwise
    // leave `.env` untracked and make the `.env` test pass for the wrong
    // reason.
    for args in [
        vec!["init", "--quiet", "--initial-branch=main"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            "fixture",
        ],
    ] {
        let status = Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(&args)
            .status()
            .expect("git available");
        assert!(status.success(), "git {args:?} failed in the fixture");
    }

    // Guard the premise of every test that expects a file to be refused:
    // if the fixture silently failed to track it, the gate would pass and
    // the assertion would be meaningless.
    let listed = Command::new("git")
        .current_dir(root)
        .args(["ls-files"])
        .output()
        .expect("git ls-files");
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    for (rel, _) in files {
        assert!(
            listed.lines().any(|line| line == *rel),
            "fixture file {rel} was not tracked, so the test would be vacuous:\n{listed}"
        );
    }

    dir
}

/// Run the gate in `--tree` mode against `root`. Returns (passed, output).
fn gate_on(root: &Path) -> (bool, String) {
    let output = Command::new(gate())
        .arg("--tree")
        .arg(root)
        .output()
        .expect("gate runs");

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));

    (output.status.success(), text)
}

#[test]
fn a_clean_tree_passes() {
    let dir = repo_with(&[
        ("README.md", "# fixture\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("deploy/dg.service", "[Service]\n"),
    ]);

    let (ok, out) = gate_on(dir.path());
    assert!(ok, "the gate refused a clean tree:\n{out}");
    assert!(
        out.contains("READY"),
        "a passing run must say so, not just exit 0:\n{out}"
    );
}

#[test]
fn a_tracked_session_transcript_fails() {
    // The exact class that shipped to a real index: a decoded working
    // session with prompts, tool output and a home path in it. Note that
    // it contains no credential-shaped string at all, which is why the
    // credential scanner reported the repository clean while this file
    // was tracked.
    let dir = repo_with(&[
        ("README.md", "# fixture\n"),
        (
            "pi-session-2026-10-22T20-39-53-378Z_01a16557.md",
            "# Pi Session\n\ncwd: /home/someone/Dev/project\n",
        ),
    ]);

    let (ok, out) = gate_on(dir.path());
    assert!(
        !ok,
        "the gate passed a tree with a tracked transcript:\n{out}"
    );
    assert!(
        out.contains("pi-session-2026-10-22T20-39-53-378Z_01a16557.md"),
        "the refusal must name the offending path:\n{out}"
    );
    assert!(
        out.contains("NOT READY"),
        "a refused run must say so:\n{out}"
    );
}

#[test]
fn a_tracked_private_key_file_fails() {
    let dir = repo_with(&[
        ("README.md", "# fixture\n"),
        (
            "keys/owner.age",
            // Assembled so the tree scan does not see a real PEM header.
            &joined(&["-----BEGIN AGE ", "ENCRYPTED FILE-----\nabc\n"]),
        ),
    ]);

    let (ok, out) = gate_on(dir.path());
    assert!(
        !ok,
        "the gate passed a tree with a tracked key file:\n{out}"
    );
    assert!(
        out.contains("keys/owner.age"),
        "the refusal must name the offending path:\n{out}"
    );
}

#[test]
fn a_tracked_environment_file_fails() {
    let dir = repo_with(&[("README.md", "# fixture\n"), (".env", "DG_TOKEN=abc\n")]);

    let (ok, out) = gate_on(dir.path());
    assert!(!ok, "the gate passed a tree with a tracked .env:\n{out}");
    assert!(
        out.contains(".env"),
        "the refusal must name the offending path:\n{out}"
    );
}

#[test]
fn tracked_build_output_fails() {
    let dir = repo_with(&[
        ("README.md", "# fixture\n"),
        ("target/debug/dg", "not really a binary\n"),
    ]);

    let (ok, out) = gate_on(dir.path());
    assert!(
        !ok,
        "the gate passed a tree with tracked build output:\n{out}"
    );
    assert!(
        out.contains("target/debug/dg"),
        "the refusal must name the offending path:\n{out}"
    );
}

#[test]
fn a_tracked_oversized_blob_fails() {
    // 300 KiB of text: over the 256 KiB threshold, and the kind of thing
    // that reaches an index by accident (an export, a log, a database).
    let body = "x".repeat(300 * 1024);
    let dir = repo_with(&[("README.md", "# fixture\n"), ("dump.txt", &body)]);

    let (ok, out) = gate_on(dir.path());
    assert!(!ok, "the gate passed a tree with an oversized blob:\n{out}");
    assert!(
        out.contains("dump.txt"),
        "the refusal must name the offending path:\n{out}"
    );
}

#[test]
fn the_report_never_prints_file_contents() {
    // A refusal that quotes the file is a new leak. The gate names paths.
    let secret = joined(&["ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"]);
    let dir = repo_with(&[
        ("README.md", "# fixture\n"),
        ("conf/leak.env", &format!("export GH_TOKEN={secret}\n")),
    ]);

    let (ok, out) = gate_on(dir.path());
    assert!(!ok, "the gate passed a tree with a credential:\n{out}");
    assert!(
        !out.contains(&secret),
        "the gate printed the credential value into its report:\n{out}"
    );
}

#[test]
fn identity_disclosure_advises_but_does_not_block() {
    // A home path and a public IP are the operator's call, not a leak.
    // A gate that blocked on them would be switched off before the first
    // publication.
    let dir = repo_with(&[
        ("README.md", "# fixture\n"),
        (
            "docs/notes.md",
            "built in /home/someone/Dev on 203.0.113.7\n",
        ),
    ]);

    let (ok, out) = gate_on(dir.path());
    assert!(ok, "identity disclosure must not block publication:\n{out}");
    assert!(
        out.contains("ADVICE"),
        "the gate must still report what publication would disclose:\n{out}"
    );
    assert!(
        out.contains("home-path"),
        "the advisory must name what it found:\n{out}"
    );
}

#[test]
fn an_unknown_option_is_a_usage_error_not_a_pass() {
    let dir = repo_with(&[("README.md", "# fixture\n")]);

    let output = Command::new(gate())
        .arg("--not-a-flag")
        .arg(dir.path())
        .output()
        .expect("gate runs");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a usage error must be distinguishable from 'ready': {output:?}"
    );
}

#[test]
fn a_missing_root_is_a_usage_error_not_a_pass() {
    let output = Command::new(gate())
        .arg("--tree")
        .arg("/nonexistent/publication-gate-root")
        .output()
        .expect("gate runs");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing root must be distinguishable from 'ready': {output:?}"
    );
}

#[test]
fn this_repository_passes_tree_mode() {
    // The gate is a gate: if db-gateway's own tracked tree is not
    // publishable, that is a finding, not a test to relax. History mode is
    // deliberately not run here -- it walks 2500 commits and belongs to a
    // pre-publication run (docs/publication-readiness.md).
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (ok, out) = gate_on(root);
    assert!(ok, "db-gateway's tracked tree is not publishable:\n{out}");
}
