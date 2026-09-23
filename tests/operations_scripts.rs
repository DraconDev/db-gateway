//! Focused contract tests for operator scripts changed by the project audit.

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

#[test]
fn backup_rejects_invalid_retention_before_touching_backup_root() {
    for keep in ["0", "-1", "many", ""] {
        let dir = tempdir();
        let root = dir.path().join("backups");
        let output = Command::new("bash")
            .args(["scripts/micro2-backup.sh", "--keep", keep, "--backup-root"])
            .arg(&root)
            .output()
            .expect("run backup script");
        assert_eq!(output.status.code(), Some(2), "keep={keep:?}");
        assert!(!root.exists(), "invalid retention created backup root");
    }
}

#[test]
fn backup_staging_is_unique_and_cleaned_on_failure() {
    let dir = tempdir();
    let root = dir.path().join("backups");
    let fake = dir.path().join("dg");
    fs::write(
        &fake,
        "#!/bin/sh\ncase \"$1\" in --version) echo 'dg 0.1.0';; list) echo '{\"databases\":[]}';; esac\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("bash")
        .args(["scripts/micro2-backup.sh", "--backup-root"])
        .arg(&root)
        .env("DG_BIN", &fake)
        .output()
        .expect("run backup script");
    assert!(!output.status.success(), "empty backup unexpectedly succeeded");
    let leftovers = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging."))
        .count();
    assert_eq!(leftovers, 0, "failed backup leaked staging directory");
}

#[test]
fn script_uses_fail_with_body_and_asserts_generation() {
    let script = fs::read_to_string("scripts/upgrade-rehearsal.sh").unwrap();
    assert!(script.contains("--fail-with-body"));
    assert!(script.contains("rows[-1][0] == \"rollback-gen\""));
    assert_eq!(script.matches("rollback-generation write intact").count(), 2);
}

#[test]
fn soak_readiness_wait_is_bounded() {
    // The fake init succeeds, then the fake serve remains alive without ever
    // logging. The bounded environment override proves readiness cannot hang.
    let dir = tempdir();
    let fake = dir.path().join("dg");
    fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = init ]; then echo '{\"owner_key\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}'; exit 0; fi\nsleep 30\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("python3")
        .args(["-c", "import runpy,sys; sys.argv=['soak.py','--dg-bin',sys.argv[1],'--seconds','0','--out',sys.argv[2]]; runpy.run_path('scripts/soak.py',run_name='__main__')"])
        .arg(&fake)
        .arg(dir.path().join("soak.log"))
        .env("SOAK_READINESS_TIMEOUT", "0.2")
        .output()
        .expect("run soak script");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("server never logged"));
}
