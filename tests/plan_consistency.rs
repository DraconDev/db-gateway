//! Focused contract tests for the generic docs consistency checker.

use std::{fs, process::Command};

fn run_checker(master: &str) -> std::process::Output {
    Command::new("python3")
        .args(["scripts/check_plan_consistency.py", master])
        .output()
        .expect("run plan consistency checker")
}

#[test]
fn generic_overview_passes() {
    let output = run_checker("docs/README.md");
    assert!(
        output.status.success(),
        "checker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn checker_rejects_missing_pre_production_warning() {
    let dir = tempfile::tempdir().unwrap();
    let source = fs::read_to_string("docs/README.md").unwrap();
    let broken = source.replacen(
        "pre-production development; it is not ready for\nproduction use or production data.",
        "production ready.",
        1,
    );
    let path = dir.path().join("broken.md");
    fs::write(&path, broken).unwrap();
    let output = run_checker(path.to_str().unwrap());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pre-production"),
        "unexpected checker output: {stderr}"
    );
}
