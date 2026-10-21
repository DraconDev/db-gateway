//! The drift gate must render the units it compares.
//!
//! `scripts/host-audit.sh` compares the unit installed on a host against
//! the repo template rendered with the reference-host values. A template
//! that gains a placeholder the render does not cover is the failure
//! these tests protect against: the expected hash is then computed over
//! text that no correctly rendered host can have, so every healthy host
//! reports a permanent false drift — and a permanently red gate cannot
//! find the real drift it exists to detect.
//!
//! The renderer is the one place that knows how a placeholder becomes a
//! value, and it refuses to emit a unit that still carries one.
//!
//! Run: `timeout 300 cargo test --locked --test unit_render`

use std::path::Path;
use std::process::Command;

/// Render `template` with `values` and return (succeeded, stdout, stderr).
fn render(template: &Path, values: &[String]) -> (bool, String, String) {
    let out = Command::new("bash")
        .arg("scripts/unit-render.sh")
        .arg(template)
        .args(values)
        .output()
        .expect("run scripts/unit-render.sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The producer's compiled-in default, e.g. `SERVER` or `KEY_FILE`.
fn producer_default(producer: &str, name: &str) -> String {
    let prefix = format!("{name}=");
    producer
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("scripts/micro2-backup.sh must define {name}"))
}

#[test]
fn a_placeholder_with_no_value_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let template = dir.path().join("unit.service");
    std::fs::write(
        &template,
        "[Service]\nExecStart=/usr/local/bin/dg --listen {{DG_LISTEN}}\n",
    )
    .unwrap();

    let (ok, stdout, stderr) = render(&template, &["DG_USER=dg".to_string()]);

    assert!(
        !ok,
        "a unit with an uncovered placeholder must be refused, not rendered: {stdout}"
    );
    assert!(
        stderr.contains("{{DG_LISTEN}}"),
        "the refusal must name the placeholder it could not cover: {stderr}"
    );
    assert!(
        stdout.is_empty(),
        "a refused render must print no unit: {stdout}"
    );
}

#[test]
fn the_values_it_is_given_replace_the_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let template = dir.path().join("unit.service");
    std::fs::write(
        &template,
        "[Service]\nUser={{DG_USER}}\nExecStart=/usr/local/bin/dg --listen {{DG_LISTEN}}\n",
    )
    .unwrap();

    let (ok, stdout, stderr) = render(
        &template,
        &[
            "DG_USER=dg".to_string(),
            "DG_LISTEN=127.0.0.1:7432".to_string(),
        ],
    );

    assert!(ok, "the render failed: {stderr}");
    assert!(
        stdout.contains("User=dg"),
        "the value did not replace the placeholder: {stdout}"
    );
    assert!(
        stdout.contains("ExecStart=/usr/local/bin/dg --listen 127.0.0.1:7432"),
        "the value did not replace the placeholder: {stdout}"
    );
    assert!(
        !stdout.contains("{{"),
        "a rendered unit must carry no placeholder: {stdout}"
    );
}

#[test]
fn the_drift_gate_renders_every_unit_it_compares() {
    // The gate compares the host's installed units against the repo
    // templates rendered with the reference-host values. A compared
    // template that gains a placeholder the value table does not cover
    // makes the render fail, and this run fail with it.
    let out = Command::new("timeout")
        .args(["200", "bash", "scripts/host-audit.sh"])
        .output()
        .expect("run scripts/host-audit.sh");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "the drift gate must render every unit it compares: {stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("host-audit must print one JSON report");
    assert_eq!(report["verdict"], "pass", "the repo-side audit must pass");
}

#[test]
fn the_rendered_backup_unit_carries_the_producers_reference_layout() {
    // The producer's compiled-in defaults are the reference layout
    // (docs/evidence/first-deploy-micro2.md records them for the
    // deployed host), so the unit the drift gate compares must be
    // rendered with exactly them.
    let producer = std::fs::read_to_string("scripts/micro2-backup.sh").unwrap();
    let server = producer_default(&producer, "SERVER");
    let listen = match server.split_once("://") {
        Some((_, host)) => host.to_string(),
        None => server.clone(),
    };
    let key_file = producer_default(&producer, "KEY_FILE");
    let data_dir = producer_default(&producer, "DATA_DIR");
    let backup_root = producer_default(&producer, "BACKUP_ROOT");

    let (ok, stdout, stderr) = render(
        Path::new("deploy/dg-backup.service"),
        &[
            "DG_USER=dg".to_string(),
            "DG_BINARY=/usr/local/bin/dg".to_string(),
            format!("DG_LISTEN={listen}"),
            format!("DG_OWNER_KEY_FILE={key_file}"),
            format!("DG_DATA_DIR={data_dir}"),
            format!("DG_BACKUP_ROOT={backup_root}"),
            "DG_BACKUP_SCRIPT=/usr/local/bin/micro2-backup.sh".to_string(),
        ],
    );

    assert!(ok, "the reference render failed: {stderr}");
    assert!(
        !stdout.contains("{{"),
        "a rendered unit must carry no placeholder: {stdout}"
    );
    let expected = format!(
        "--server http://{listen} --key-file {key_file} --data-dir {data_dir} --backup-root {backup_root}"
    );
    assert!(
        stdout.contains(&expected),
        "the rendered unit must drive the producer with the reference paths.\n  expected: {expected}\n  rendered: {stdout}"
    );
}

#[test]
fn a_unit_that_gains_a_placeholder_fails_the_drift_gate() {
    // The auditor's scenario, in a disposable fixture: a compared unit
    // carries a placeholder the value table does not cover. The gate
    // must fail loudly and name it, rather than compute an expected
    // hash over the literal "{{...}}" text — that hash matches no
    // correctly rendered host, so the gate would report a permanent
    // false drift on every healthy host.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in ["scripts", "deploy"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    for file in [
        "scripts/host-audit.sh",
        "scripts/unit-render.sh",
        "scripts/micro2-backup.sh",
        "deploy/dg-backup.service",
        "deploy/dg-backup.timer",
        "deploy/dg-backup-digest.service",
        "deploy/dg-backup-digest.timer",
    ] {
        std::fs::copy(file, root.join(file)).unwrap();
    }

    // A throwaway repository: the gate reads the templates at git HEAD.
    // The project's own identity is used when it has one, so no
    // identity is invented; the fixture commit never enters the
    // project's history.
    let name = std::process::Command::new("git")
        .args(["config", "--get", "user.name"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unit-render fixture".to_string());
    let email = std::process::Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "fixture@invalid.invalid".to_string());
    let git = |args: &[&str]| -> bool {
        Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(git(&["init", "-q"]), "git init must succeed");
    assert!(git(&["add", "-A"]), "git add must succeed");
    assert!(
        git(&[
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "-q",
            "-m",
            "fixture"
        ]),
        "the fixture commit must succeed"
    );

    // Baseline: the fixture mirrors HEAD's templates, so the gate is
    // green — the gate is not simply broken.
    let baseline = Command::new("timeout")
        .current_dir(root)
        .args(["200", "bash", "scripts/host-audit.sh"])
        .output()
        .expect("run the fixture gate");
    assert!(
        baseline.status.success(),
        "the fixture gate must pass before the template changes: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );

    // Now the unit gains a placeholder nobody has a value for.
    let unit = root.join("deploy/dg-backup.service");
    let mut text = std::fs::read_to_string(&unit).unwrap();
    text.push_str("Environment=DG_EXTRA={{DG_NOT_A_KNOWN_VALUE}}\n");
    std::fs::write(&unit, text).unwrap();
    assert!(git(&["add", "-A"]), "git add must succeed");
    assert!(
        git(&[
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "-q",
            "-m",
            "fixture"
        ]),
        "the fixture commit must succeed"
    );

    let broken = Command::new("timeout")
        .current_dir(root)
        .args(["200", "bash", "scripts/host-audit.sh"])
        .output()
        .expect("run the fixture gate");
    let stderr = String::from_utf8_lossy(&broken.stderr).to_string();
    assert!(
        !broken.status.success(),
        "the gate must fail when a compared unit carries an uncovered placeholder"
    );
    assert!(
        stderr.contains("{{DG_NOT_A_KNOWN_VALUE}}"),
        "the failure must name the placeholder: {stderr}"
    );
}
