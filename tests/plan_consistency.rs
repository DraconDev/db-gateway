//! Focused contract tests for the canonical roadmap checker.

use std::{fs, process::Command};

fn run_checker(master: &str) -> std::process::Output {
    Command::new("python3")
        .args(["scripts/check_plan_consistency.py", master])
        .output()
        .expect("run plan consistency checker")
}

#[test]
fn canonical_plan_passes() {
    let output = run_checker("docs/personal-turso-megaplan.md");
    assert!(
        output.status.success(),
        "checker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn checker_rejects_missing_phase_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let source = fs::read_to_string("docs/personal-turso-megaplan.md").unwrap();
    let broken = source.replacen(
        "Rollback / stop rule:\n\n- no runtime mutation occurs in this phase;",
        "Notes:\n\n- no runtime mutation occurs in this phase;",
        1,
    );
    let path = dir.path().join("broken.md");
    fs::write(&path, broken).unwrap();
    let output = run_checker(path.to_str().unwrap());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("phase-rollback"),
        "unexpected checker output: {stderr}"
    );
}
