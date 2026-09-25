//! Focused contract tests for operator scripts changed by the project audit.

use sha2::Digest;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, thread, time::Duration};

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
    assert!(
        !output.status.success(),
        "empty backup unexpectedly succeeded"
    );
    let leftovers = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging."))
        .count();
    assert_eq!(leftovers, 0, "failed backup leaked staging directory");
}

#[test]
fn backup_uses_array_invocation_and_emits_contract_artifacts() {
    let dir = tempdir();
    let root = dir.path().join("backup root with spaces");
    let data = dir.path().join("data with spaces");
    let key = dir.path().join("owner key with spaces");
    fs::create_dir_all(&data).unwrap();
    fs::write(&key, "a".repeat(64)).unwrap();
    let fake = dir.path().join("dg binary with spaces");
    fs::write(
        &fake,
        r#"#!/usr/bin/env bash
set -eu
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[$i]}" in
    --version) echo 'dg 0.1.0'; exit 0;;
    list) echo '{"databases":["db"]}'; exit 0;;
    export)
      out=""
      for ((j=i+1; j<${#args[@]}; j++)); do
        if [ "${args[$j]}" = "--out" ]; then out="${args[$((j+1))]}"; fi
      done
      python3 - "$out" <<'PY'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('create table payload(value integer)')
con.execute('insert into payload values (7)')
con.commit()
con.close()
PY
      exit 0;;
  esac
done
exit 2
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let metadata = rusqlite::Connection::open(data.join("metadata.sqlite")).unwrap();
    metadata
        .execute_batch("create table databases(name text); insert into databases values ('db');")
        .unwrap();
    drop(metadata);
    let output = Command::new("bash")
        .args(["scripts/micro2-backup.sh", "--data-dir"])
        .arg(&data)
        .args(["--backup-root"])
        .arg(&root)
        .args(["--key-file"])
        .arg(&key)
        .env("DG_BIN", &fake)
        .output()
        .expect("run backup script");
    assert!(
        output.status.success(),
        "backup failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let set = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_type().unwrap().is_dir()
                && entry.file_name().to_string_lossy().starts_with('2')
        })
        .expect("published set");
    let set = set.path();
    for name in [
        "manifest.json",
        "metadata.sqlite.bak",
        "db.sqlite",
        "db.sha256",
    ] {
        assert!(set.join(name).is_file(), "missing {name}");
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(set.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["databases"][0]["name"], "db");
    assert_eq!(manifest["databases"][0]["file"], "db.sqlite");
    assert_eq!(manifest["registry"]["file"], "metadata.sqlite.bak");
    let sidecar = fs::read_to_string(set.join("db.sha256")).unwrap();
    let digest = sha2::Sha256::digest(fs::read(set.join("db.sqlite")).unwrap());
    assert_eq!(sidecar.trim(), hex::encode(digest));
}

#[test]
fn failed_restore_drill_terminates_its_server() {
    let dir = tempdir();
    let pid_file = dir.path().join("server.pid");
    let fake = dir.path().join("failing dg");
    fs::write(
        &fake,
        r#"#!/usr/bin/env bash
set -eu
if [ "$1" = init ]; then
  echo '{"owner_key":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}'
  exit 0
fi
if [ "$1" = serve ]; then
  echo $$ > "$PIDFILE"
  printf '%s\n' '{"fields":{"address":"127.0.0.1:1"}}' >&2
  exec sleep 30
fi
exit 2
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("python3")
        .args(["scripts/wipe_restore.py"])
        .arg(&fake)
        .env("PIDFILE", &pid_file)
        .output()
        .expect("run failing restore drill");
    assert!(!output.status.success());
    let pid = fs::read_to_string(&pid_file).unwrap();
    let pid = pid.trim();
    for _ in 0..40 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("restore drill left server process {pid} running");
}

#[test]
fn script_uses_fail_with_body_and_asserts_generation() {
    let script = fs::read_to_string("scripts/upgrade-rehearsal.sh").unwrap();
    assert!(script.contains("--fail-with-body"));
    assert!(script.contains("rows[-1][0] == \"rollback-gen\""));
    assert_eq!(
        script.matches("rollback-generation write intact").count(),
        2
    );
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

#[test]
fn setup_help_documents_interface_without_building() {
    let output = Command::new("bash")
        .args(["scripts/setup.sh", "--help"])
        .output()
        .expect("run setup script");
    assert!(output.status.success());
    let usage = String::from_utf8_lossy(&output.stderr);
    for flag in ["--prefix", "--smoke", "--no-install", "--help"] {
        assert!(usage.contains(flag), "missing {flag}: {usage}");
    }
}

#[test]
fn setup_rejects_unknown_arguments_before_doing_anything() {
    let output = Command::new("bash")
        .args(["scripts/setup.sh", "--deploy-production"])
        .output()
        .expect("run setup script");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown argument"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
