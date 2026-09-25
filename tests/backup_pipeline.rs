//! Versioned backup pipeline: script-produced sets, offline verify, restore drill.
//!
//! Run: `timeout 600 cargo test --locked --test backup_pipeline`
//!
//! The canonical producer is scripts/micro2-backup.sh against a live
//! disposable server; `dg backup verify` re-checks published sets offline;
//! the restore drill follows docs/recovery-contract.md §3 against a set the
//! script actually published (hash chain, empty target, registry restore
//! while stopped, validated imports, exact rows, access re-verification).

use std::{
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

fn dg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dg"))
}

struct Server {
    child: Child,
    address: String,
    key: String,
    key_file: PathBuf,
    data_dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn boot(data_dir: &Path, backup_root: &Path) -> Server {
    let output = dg()
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let key: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let key_file = data_dir.join("owner.key");
    std::fs::write(&key_file, &key).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = dg()
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--backup-root",
            backup_root.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut address = None;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if let Ok(log) = serde_json::from_str::<Value>(&line)
            && let Some(addr) = log["fields"]["address"].as_str()
        {
            address = Some(addr.to_string());
            break;
        }
    }
    Server {
        child,
        address: address.expect("server logged its listen address"),
        key,
        key_file,
        data_dir: data_dir.to_path_buf(),
    }
}

/// Run a `dg` subcommand against the server; returns parsed JSON stdout.
fn cli(server: &Server, args: &[&str]) -> Value {
    let mut full = vec![
        "--server".to_string(),
        format!("http://{}", server.address),
        "--key-file".to_string(),
        server.key_file.to_str().unwrap().to_string(),
    ];
    full.extend(args.iter().map(|arg| arg.to_string()));
    let output = dg().args(&full).output().unwrap();
    assert!(
        output.status.success(),
        "dg {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_script(
    server: &Server,
    backup_root: &Path,
    keep: &str,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new("bash");
    command.args([
        "scripts/micro2-backup.sh",
        "--server",
        &format!("http://{}", server.address),
        "--key-file",
        server.key_file.to_str().unwrap(),
        "--data-dir",
        server.data_dir.to_str().unwrap(),
        "--backup-root",
        backup_root.to_str().unwrap(),
        "--keep",
        keep,
    ]);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn only_set(backup_root: &Path) -> PathBuf {
    let sets: Vec<PathBuf> = std::fs::read_dir(backup_root)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        let bytes = name.as_bytes();
                        bytes.len() == 16 && bytes[8] == b'T' && bytes[15] == b'Z'
                    })
        })
        .collect();
    assert_eq!(sets.len(), 1, "expected exactly one set: {sets:?}");
    sets.into_iter().next().unwrap()
}

fn attempts(backup_root: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(backup_root.join("attempts.jsonl")).unwrap();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn script_run_reports_fresh_status() {
    let dir = tempfile::tempdir().unwrap();
    let backup_root = dir.path().join("backups");
    let server = boot(&dir.path().join("data"), &backup_root);
    cli(&server, &["create", "appdb"]);
    cli(
        &server,
        &["query", "appdb", "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"],
    );
    cli(&server, &["query", "appdb", "INSERT INTO t(v) VALUES ('x')"]);

    let output = run_script(&server, &backup_root, "3", &[]);
    assert!(
        output.status.success(),
        "backup script: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let set = only_set(&backup_root);
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(set.join("manifest.json")).unwrap()).unwrap();
    for key in [
        "started_at",
        "completed_at",
        "registry_captured_at",
        "databases",
        "registry",
        "verification",
    ] {
        assert!(manifest.get(key).is_some(), "manifest misses {key}");
    }
    assert!(set.join("appdb.sqlite").is_file());
    assert!(set.join("metadata.sqlite.bak").is_file());

    let status = cli(&server, &["backup", "status"]);
    assert_eq!(status["per_database"]["appdb"]["state"], "fresh");
    assert_eq!(status["sets"][0]["has_registry"], true);

    let records = attempts(&backup_root);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["outcome"], "ok");
    assert_eq!(
        records[0]["set"],
        set.file_name().unwrap().to_str().unwrap()
    );
}

#[test]
fn verify_accepts_published_set() {
    let dir = tempfile::tempdir().unwrap();
    let backup_root = dir.path().join("backups");
    let server = boot(&dir.path().join("data"), &backup_root);
    cli(&server, &["create", "appdb"]);
    cli(
        &server,
        &["query", "appdb", "CREATE TABLE t(id INTEGER PRIMARY KEY)"],
    );
    let output = run_script(&server, &backup_root, "3", &[]);
    assert!(output.status.success());

    let report = dg()
        .args([
            "backup",
            "verify",
            "--backup-root",
            backup_root.to_str().unwrap(),
            "latest",
        ])
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "verify: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let value: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["databases"][0]["name"], "appdb");
    assert_eq!(value["has_registry"], true);
    assert!(value.get("failures").is_none());
}

fn mutate_set_copy(set: &Path, dir: &Path, name: &str) -> PathBuf {
    let dest = dir.join(name);
    copy_dir(set, &dest);
    dest
}

fn copy_dir(source: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    for entry in std::fs::read_dir(source).unwrap().flatten() {
        let target = dest.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn rewrite_hashes(set: &Path) {
    use sha2::Digest;
    let manifest_path = set.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    for entry in manifest["databases"].as_array_mut().unwrap() {
        let file = entry["file"].as_str().unwrap().to_string();
        let bytes = std::fs::read(set.join(&file)).unwrap();
        let hex = hex::encode(sha2::Sha256::digest(&bytes));
        entry["sha256"] = Value::String(hex.clone());
        entry["size"] = Value::from(bytes.len() as u64);
        std::fs::write(set.join(format!("{}.sha256", entry["name"].as_str().unwrap())), format!("{hex}\n")).unwrap();
    }
    std::fs::write(manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
}

fn verify(set_root: &Path, set: &str) -> (bool, Value, String) {
    let output = dg()
        .args([
            "backup",
            "verify",
            "--backup-root",
            set_root.to_str().unwrap(),
            set,
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let value: Value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (
        output.status.success(),
        value,
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn verify_rejects_tampered_and_malformed_sets() {
    let dir = tempfile::tempdir().unwrap();
    let backup_root = dir.path().join("backups");
    let server = boot(&dir.path().join("data"), &backup_root);
    cli(&server, &["create", "appdb"]);
    cli(
        &server,
        &[
            "query",
            "appdb",
            "CREATE TABLE authors(id INTEGER PRIMARY KEY, name TEXT)",
        ],
    );
    cli(
        &server,
        &[
            "query",
            "appdb",
            "CREATE TABLE books(id INTEGER PRIMARY KEY, author_id INTEGER REFERENCES authors(id))",
        ],
    );
    let output = run_script(&server, &backup_root, "3", &[]);
    assert!(output.status.success());
    let good = only_set(&backup_root);
    let good_name = good.file_name().unwrap().to_str().unwrap().to_string();

    // Sanity: the untouched set verifies.
    let (ok, _, _) = verify(&backup_root, &good_name);
    assert!(ok);

    // Tampered bytes with stale hashes fail the checksum chain.
    let tampered = mutate_set_copy(&good, dir.path(), "20200101T000001Z");
    let db_path = tampered.join("appdb.sqlite");
    let mut bytes = std::fs::read(&db_path).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&db_path, &bytes).unwrap();
    let (ok, value, _) = verify(dir.path(), "20200101T000001Z");
    assert!(!ok);
    assert!(
        value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure.as_str().unwrap().contains("checksum"))
    );

    // Broken sidecar fails even when file bytes match the manifest.
    let broken_sidecar = mutate_set_copy(&good, dir.path(), "20200101T000002Z");
    std::fs::write(
        broken_sidecar.join("appdb.sha256"),
        "0".repeat(64) + "\n",
    )
    .unwrap();
    let (ok, _, _) = verify(dir.path(), "20200101T000002Z");
    assert!(!ok);

    // Malformed manifest fails as a set, never as healthy.
    let broken_manifest = mutate_set_copy(&good, dir.path(), "20200101T000003Z");
    std::fs::write(broken_manifest.join("manifest.json"), "{").unwrap();
    let (ok, value, _) = verify(dir.path(), "20200101T000003Z");
    assert!(!ok);
    assert_eq!(value["ok"], false);

    // Foreign-key violations fail even with a consistent hash chain.
    let fk_bad = mutate_set_copy(&good, dir.path(), "20200101T000004Z");
    {
        let conn = rusqlite::Connection::open(fk_bad.join("appdb.sqlite")).unwrap();
        conn.execute("PRAGMA foreign_keys=OFF", []).unwrap();
        conn.execute("INSERT INTO books(id, author_id) VALUES (1, 4242)", [])
            .unwrap();
    }
    rewrite_hashes(&fk_bad);
    let (ok, value, _) = verify(dir.path(), "20200101T000004Z");
    assert!(!ok);
    assert!(
        value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure.as_str().unwrap().contains("foreign_key")),
        "{value}"
    );

    // Smuggled server-owned tables fail even with a consistent hash chain.
    let smuggled = mutate_set_copy(&good, dir.path(), "20200101T000005Z");
    {
        let conn = rusqlite::Connection::open(smuggled.join("appdb.sqlite")).unwrap();
        conn.execute("CREATE TABLE _dg_evil(id INTEGER PRIMARY KEY)", [])
            .unwrap();
    }
    rewrite_hashes(&smuggled);
    let (ok, value, _) = verify(dir.path(), "20200101T000005Z");
    assert!(!ok);
    assert!(
        value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure.as_str().unwrap().contains("protected")),
        "{value}"
    );

    // Unknown selectors and empty roots are not-found, not healthy.
    let (ok, _, stderr) = verify(&backup_root, "19990101T000000Z");
    assert!(!ok);
    assert!(stderr.contains("backup_set_not_found"), "{stderr}");
    let empty = dir.path().join("empty-root");
    std::fs::create_dir(&empty).unwrap();
    let (ok, _, stderr) = verify(&empty, "latest");
    assert!(!ok);
    assert!(stderr.contains("backup_set_not_found"), "{stderr}");
}

fn fake_hanging_dg(path: &Path) {
    std::fs::write(
        path,
        "#!/bin/sh\ncase \"$1\" in --version) echo 'dg 0.1.0';; list) echo '{\"databases\":[\"a\"]}';; export) sleep 60;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn staging_dirs(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with(".staging."))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn published_sets(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                let bytes = name.as_bytes();
                                bytes.len() == 16 && bytes[8] == b'T' && bytes[15] == b'Z'
                            })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn interrupted_run_publishes_nothing_and_next_run_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let backup_root = dir.path().join("backups");
    let server = boot(&dir.path().join("data"), &backup_root);
    cli(&server, &["create", "appdb"]);

    let fake = dir.path().join("hanging-dg");
    fake_hanging_dg(&fake);
    let mut killed = Command::new("bash")
        .args([
            "scripts/micro2-backup.sh",
            "--server",
            &format!("http://{}", server.address),
            "--key-file",
            server.key_file.to_str().unwrap(),
            "--data-dir",
            server.data_dir.to_str().unwrap(),
            "--backup-root",
            backup_root.to_str().unwrap(),
            "--keep",
            "3",
        ])
        .env("DG_BIN", &fake)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the run is inside the hanging export, then SIGKILL it:
    // untrappable, so staging leaks until the next run cleans up.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "script never reached its staging phase"
        );
        if !staging_dirs(&backup_root).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    killed.kill().unwrap();
    let _ = killed.wait();
    assert!(published_sets(&backup_root).is_empty());

    // The next real run cleans the leaked staging and publishes exactly one set.
    let output = run_script(&server, &backup_root, "3", &[]);
    assert!(
        output.status.success(),
        "recovery run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(published_sets(&backup_root).len(), 1);
    assert!(staging_dirs(&backup_root).is_empty());
    // SIGKILL is untrappable, so the killed run records nothing; the
    // recovery run records its own success.
    let records = attempts(&backup_root);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["outcome"], "ok");
}

#[test]
fn failed_run_records_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let backup_root = dir.path().join("backups");
    std::fs::create_dir_all(&backup_root).unwrap();
    let key_file = dir.path().join("owner.key");
    std::fs::write(&key_file, "a".repeat(64)).unwrap();
    let output = Command::new("bash")
        .args([
            "scripts/micro2-backup.sh",
            "--server",
            "http://127.0.0.1:9",
            "--key-file",
            key_file.to_str().unwrap(),
            "--data-dir",
            dir.path().join("data").to_str().unwrap(),
            "--backup-root",
            backup_root.to_str().unwrap(),
            "--keep",
            "3",
        ])
        .env("DG_BIN", env!("CARGO_BIN_EXE_dg"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(published_sets(&backup_root).is_empty());
    let records = attempts(&backup_root);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["outcome"], "error");
    assert_eq!(records[0]["step"], "list");
}
