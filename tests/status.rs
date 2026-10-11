//! Operator visibility: health, status counters, backup freshness.
//!
//! Run: `timeout 300 cargo test --locked --test status`
//!
//! Pins the AI-operator surface: unauthenticated liveness, owner-only
//! `/v1/status` shape + admission config + request counting (including a
//! forced 429), and `/v1/backups` fresh/stale/unknown transitions against
//! crafted versioned sets.

use std::{
    fs,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

struct Server {
    child: Child,
    address: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn init(data_dir: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string()
}

fn start(data_dir: &std::path::Path, extra: &[&str]) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dg"));
    cmd.args([
        "serve",
        "--listen",
        "127.0.0.1:0",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    cmd.args(extra);
    let mut child = cmd.stderr(Stdio::piped()).spawn().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut address = None;
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
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
    }
}

fn get(address: &str, path: &str, cred: Option<&str>) -> (u16, Value) {
    let client = reqwest::blocking::Client::new();
    let mut req = client.get(format!("http://{address}{path}"));
    if let Some(cred) = cred {
        req = req.bearer_auth(cred);
    }
    let response = req.timeout(Duration::from_secs(30)).send().unwrap();
    let status = response.status().as_u16();
    let body = response.json().unwrap_or(Value::Null);
    (status, body)
}

fn post(address: &str, path: &str, cred: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(cred)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn health_is_unauthenticated_liveness_only() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let server = start(dir.path(), &[]);
    let (status, body) = get(&server.address, "/health/live", None);
    assert_eq!(status, 200);
    assert_eq!(body, json!({"status": "ok"}));
}

#[test]
fn status_shape_counters_and_owner_gate() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path(), &[]);
    // Unauthenticated and app-key callers are refused; owner sees all.
    let (status, _) = get(&server.address, "/v1/status", None);
    assert_eq!(status, 401);
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "sdb"}),
    );
    assert_eq!(status, 200);
    let (status, key) = post(
        &server.address,
        "/v1/keys",
        &owner,
        &json!({"name": "rk", "role": "database_read_write", "databases": ["sdb"]}),
    );
    // Key issue shape: {"key": ...} — tolerate the exact key field only.
    assert_eq!(status, 200);
    let app_key = key["key"].as_str().unwrap().to_string();
    let (status, _) = get(&server.address, "/v1/status", Some(&app_key));
    assert_eq!(status, 403);

    let (status, before) = get(&server.address, "/v1/status", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(before["version"], json!(env!("CARGO_PKG_VERSION")));
    assert_eq!(before["admission"]["read_permits"], json!(8));
    assert_eq!(before["admission"]["write_permits"], json!(4));
    assert_eq!(before["admission"]["acquire_timeout_ms"], json!(500));
    let total_before = before["requests"]["total"].as_u64().unwrap();
    for _ in 0..3 {
        let (status, _) = post(
            &server.address,
            "/v1/databases/sdb/query",
            &owner,
            &json!({"sql": "SELECT 1", "args": []}),
        );
        assert_eq!(status, 200);
    }
    let (status, after) = get(&server.address, "/v1/status", Some(&owner));
    assert_eq!(status, 200);
    // Three queries plus this status call itself.
    assert_eq!(
        after["requests"]["total"].as_u64().unwrap(),
        total_before + 4
    );
    assert!(after["uptime_secs"].as_u64().is_some());
}

/// Slow write that holds its permits into the 2 s execution deadline.
const SLOW_WRITE: &str =
    "INSERT INTO t WITH RECURSIVE c(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM c) SELECT v FROM c";

#[test]
fn forced_429_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path(), &[]);
    for db in ["w0", "w1", "w2", "w3"] {
        let (status, _) = post(
            &server.address,
            "/v1/databases",
            &owner,
            &json!({"name": db}),
        );
        assert_eq!(status, 200);
        let (status, _) = post(
            &server.address,
            &format!("/v1/databases/{db}/query"),
            &owner,
            &json!({"sql": "CREATE TABLE t (v INTEGER)", "args": []}),
        );
        assert_eq!(status, 200);
    }
    // Four slow writes exhaust the write lane; a fifth write must shed.
    let holders: Vec<_> = ["w0", "w1", "w2", "w3"]
        .into_iter()
        .map(|db| {
            let address = server.address.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                post(
                    &address,
                    &format!("/v1/databases/{db}/query"),
                    &owner,
                    &json!({"sql": SLOW_WRITE, "args": []}),
                )
            })
        })
        .collect();
    // Let the holders take their permits, then probe until the shed shows.
    std::thread::sleep(Duration::from_millis(300));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let observed = loop {
        let (status, _) = post(
            &server.address,
            "/v1/databases/w0/query",
            &owner,
            &json!({"sql": "INSERT INTO t VALUES (1)", "args": []}),
        );
        if status == 429 {
            break true;
        }
        assert_eq!(status, 200);
        assert!(
            std::time::Instant::now() < deadline,
            "write lane never shed under four slow holders"
        );
    };
    assert!(observed);
    for h in holders {
        let _ = h.join().unwrap();
    }
    let (status, body) = get(&server.address, "/v1/status", Some(&owner));
    assert_eq!(status, 200);
    assert!(
        body["requests"]["capacity_exceeded"].as_u64().unwrap() >= 1,
        "forced 429 must be counted: {body}"
    );
}

/// Craft a versioned set dir with the script's MANIFEST layout.
fn craft_set(root: &std::path::Path, name: &str, dbs: &[&str], mtime: Option<SystemTime>) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let started_at = mtime
        .unwrap_or_else(SystemTime::now)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let manifest = json!({
        "timestamp": name,
        "started_at": started_at,
        "dg_version": "0.1.0",
        "databases": dbs.iter().map(|db| json!({"name": db, "size": 1, "sha256": "x"})).collect::<Vec<_>>(),
        "registry": {"file": "metadata.sqlite.bak", "size": 1, "sha256": "y"},
    });
    fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    fs::write(dir.join("metadata.sqlite.bak"), b"registry").unwrap();
    if let Some(mtime) = mtime {
        fs::File::options()
            .read(true)
            .open(&dir)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
}

#[test]
fn backups_freshness_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let backup_root = dir.path().join("sets");
    fs::create_dir_all(&backup_root).unwrap();
    let server = start(
        dir.path(),
        &["--backup-root", backup_root.to_str().unwrap()],
    );
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "bdb"}),
    );
    assert_eq!(status, 200);
    let (status, _) = post(
        &server.address,
        "/v1/databases/bdb/query",
        &owner,
        &json!({"sql": "CREATE TABLE t (v INTEGER)", "args": []}),
    );
    assert_eq!(status, 200);
    // Written but in no set: unknown (never implied healthy).
    let (status, body) = get(&server.address, "/v1/backups", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(body["per_database"]["bdb"]["state"], json!("unknown"));
    // A set whose coverage began after the write is fresh.
    craft_set(
        &backup_root,
        "20260922T223715Z",
        &["bdb"],
        Some(SystemTime::now() + Duration::from_secs(2)),
    );
    let (status, body) = get(&server.address, "/v1/backups", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(body["per_database"]["bdb"]["state"], json!("fresh"));
    assert_eq!(
        body["per_database"]["bdb"]["newest_set"],
        json!("20260922T223715Z")
    );
    assert_eq!(body["sets"][0]["has_registry"], json!(true));
    // A write newer than the newest set is stale. Backdate a second set so
    // the comparison is deterministic regardless of mtime granularity.
    craft_set(
        &backup_root,
        "20200101T000000Z",
        &["bdb"],
        Some(UNIX_EPOCH + Duration::from_secs(1000)),
    );
    let (status, _) = post(
        &server.address,
        "/v1/databases/bdb/query",
        &owner,
        &json!({"sql": "INSERT INTO t VALUES (1)", "args": []}),
    );
    assert_eq!(status, 200);
    std::fs::remove_dir_all(backup_root.join("20260922T223715Z")).unwrap();
    let (status, body) = get(&server.address, "/v1/backups", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(body["per_database"]["bdb"]["state"], json!("stale"));
    // Staging dirs, junk, and one malformed versioned member never hide a
    // valid set or turn the whole scan unknown.
    fs::create_dir_all(backup_root.join(".staging-x")).unwrap();
    fs::write(backup_root.join("notes.txt"), "nope").unwrap();
    let malformed = backup_root.join("20191231T235959Z");
    fs::create_dir_all(&malformed).unwrap();
    fs::write(malformed.join("manifest.json"), "{").unwrap();
    let (status, body) = get(&server.address, "/v1/backups", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(body["sets"].as_array().unwrap().len(), 1);
    fs::remove_dir_all(malformed).unwrap();

    // The durable stale marker survives restart; losing process memory must
    // never make the older containing set look fresh again.
    drop(server);
    let server = start(
        dir.path(),
        &["--backup-root", backup_root.to_str().unwrap()],
    );
    let (status, body) = get(&server.address, "/v1/backups", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(body["per_database"]["bdb"]["state"], json!("stale"));
    // App keys are refused the operational surface.
    let (status, key) = post(
        &server.address,
        "/v1/keys",
        &owner,
        &json!({"name": "bk", "role": "database_read_only", "databases": ["bdb"]}),
    );
    assert_eq!(status, 200);
    let app_key = key["key"].as_str().unwrap();
    let (status, _) = get(&server.address, "/v1/backups", Some(app_key));
    assert_eq!(status, 403);
}

#[test]
fn backups_unreadable_root_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let missing = dir.path().join("no-such-root");
    let server = start(dir.path(), &["--backup-root", missing.to_str().unwrap()]);
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "udb"}),
    );
    assert_eq!(status, 200);
    let (status, body) = get(&server.address, "/v1/backups", Some(&owner));
    assert_eq!(status, 200);
    assert_eq!(body["backup_root"], json!(missing.to_str().unwrap()));
    assert_eq!(body["sets"].as_array().unwrap().len(), 0);
    assert_eq!(body["per_database"]["udb"]["state"], json!("unknown"));
}

#[test]
fn status_reports_per_database_usage_since_boot() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path(), &[]);
    let usage = || -> Value {
        let (status, body) = get(&server.address, "/v1/status", Some(&owner));
        assert_eq!(status, 200);
        body["databases"].clone()
    };
    assert_eq!(usage(), json!({}));

    let query = |db: &str, sql: &str| -> (u16, Value) {
        post(
            &server.address,
            &format!("/v1/databases/{db}/query"),
            &owner,
            &json!({"sql": sql, "args": []}),
        )
    };
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "udb"}),
    );
    assert_eq!(status, 200);
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "idle"}),
    );
    assert_eq!(status, 200);
    // DDL counting is deliberately not pinned: snapshot after setup and
    // assert exact deltas for the measured statements only.
    let (status, _) = query("udb", "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)");
    assert_eq!(status, 200);
    let before = usage();
    let (status, _) = query("udb", "INSERT INTO t(v) VALUES ('a'), ('b'), ('c')");
    assert_eq!(status, 200);
    let (status, _) = query("udb", "SELECT * FROM t WHERE v <> 'a'");
    assert_eq!(status, 200);
    let (status, _) = post(
        &server.address,
        "/v1/databases/udb/batch",
        &owner,
        &json!({
            "atomic": true,
            "statements": [
                {"sql": "SELECT * FROM t", "args": []},
                {"sql": "INSERT INTO t(v) VALUES ('d')", "args": []},
            ],
        }),
    );
    assert_eq!(status, 200);
    // Failed statements record nothing.
    let (status, _) = query("udb", "SELECT * FROM no_such_table");
    assert_eq!(status, 400);
    let after = usage();
    let delta =
        |key: &str| after["udb"][key].as_u64().unwrap() - before["udb"][key].as_u64().unwrap_or(0);
    assert_eq!(delta("rows_read"), 2 + 3);
    assert_eq!(delta("rows_written"), 3 + 1);
    // Every admission pass is measured, including the failed statement's
    // (the wait happened even though no rows were counted).
    assert_eq!(delta("queue_waits"), 4);
    assert_eq!(delta("queue_timeouts"), 0);
    assert!(after["udb"]["queue_wait_ms"].as_u64().is_some());
    // Untouched databases stay absent.
    assert!(after.get("idle").is_none());
}

fn delete(address: &str, path: &str, cred: &str) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .delete(format!("http://{address}{path}"))
        .bearer_auth(cred)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn failed_owner_writes_pin_no_phantom_status_rows() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path(), &[]);
    // Three ways a name gets admitted and is then never created: a delete of
    // a name with no registry row, a clone whose source is missing, and an
    // import whose payload fails validation. Each is refused — and none may
    // leave the per-name admission/usage state behind for /v1/status to
    // report as a database that never existed.
    let (status, _) = delete(&server.address, "/v1/databases/ghost", &owner);
    assert_eq!(status, 404);
    let (status, _) = post(
        &server.address,
        "/v1/databases/nosuch/clone",
        &owner,
        &json!({"destination": "ghost_clone"}),
    );
    assert_eq!(status, 404);
    let (status, _) = post(
        &server.address,
        "/v1/databases/src/import",
        &owner,
        &json!({"name": "ghost_import", "data_base64": "bm90IGEgc3FsaXRlIGRhdGFiYXNl"}),
    );
    assert_eq!(status, 400);
    let (status, body) = get(&server.address, "/v1/status", Some(&owner));
    assert_eq!(status, 200);
    for ghost in ["ghost", "ghost_clone", "ghost_import"] {
        assert!(
            body["databases"].get(ghost).is_none(),
            "{ghost} pinned a phantom status row: {body}"
        );
    }
}
