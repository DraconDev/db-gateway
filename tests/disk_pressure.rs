//! Disk-pressure drill: the database file cannot grow, so the growing write
//! fails the way a full disk fails it.
//!
//! Run: `timeout 120 cargo test --locked --test disk_pressure`
//!
//! A real `dg serve` serves a database whose file cannot grow (a persistent
//! `max_page_count` ceiling imposed from outside the server, standing in
//! for a full disk: SQLite reports the same `SQLITE_FULL` either way). The
//! growing write must fail with the stable storage error while acknowledged
//! rows stay exact, other databases keep serving, and lifting the ceiling
//! recovers writes. Synchronization is HTTP acknowledgement only — no sleeps.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;

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

/// Spawn `dg serve` on an existing data dir; returns once the server logs
/// its listen address (readiness signal, not a timer).
fn spawn_on(data_dir: &std::path::Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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

fn request(address: &str, method: &str, path: &str, key: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(key)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn page_count_ceiling_fails_the_growing_write_and_lifts_cleanly() {
    let data_dir = TempDir::new().unwrap();
    let server = spawn_on(data_dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    for db in ["pressure", "control"] {
        let (status, body) = request(
            &server.address,
            "POST",
            "/v1/databases",
            &owner,
            &json!({"name": db}),
        );
        assert_eq!(status, 200, "create {db}: {body}");
        let (status, body) = request(
            &server.address,
            "POST",
            &format!("/v1/databases/{db}/query"),
            &owner,
            &json!({"sql": "CREATE TABLE payload (id INTEGER PRIMARY KEY, blob TEXT NOT NULL)"}),
        );
        assert_eq!(status, 200, "ddl {db}: {body}");
    }
    for i in 0..3 {
        let (status, body) = request(
            &server.address,
            "POST",
            "/v1/databases/pressure/query",
            &owner,
            &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": [format!("base-{i}")]}),
        );
        assert_eq!(status, 200, "baseline {i}: {body}");
    }
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/control/query",
        &owner,
        &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": ["control-row"]}),
    );
    assert_eq!(status, 200, "control baseline: {body}");

    // Impose the ceiling from outside the server, the way a full disk
    // imposes itself: a persistent PRAGMA on the database file. Baseline
    // (schema + 3 small rows) fits in a handful of 4 KiB pages; 8 pages
    // leaves no room for the 100 KiB write below. SQLite reports the same
    // SQLITE_FULL for this ceiling as for a genuinely full disk.
    {
        let conn =
            rusqlite::Connection::open(data_dir.path().join("pressure.sqlite")).unwrap();
        conn.execute_batch("PRAGMA max_page_count=8;").unwrap();
        let ceiling: i64 = conn
            .query_row("PRAGMA max_page_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ceiling, 8);
    }

    // Grow the file past the ceiling with small writes, each far under the
    // 64 KiB JSON body wall: the write that needs page 9 must fail with the
    // stable storage error — a response, not a dropped connection or a panic.
    let chunk = "y".repeat(4_000);
    let mut full_at = None;
    for i in 0..20 {
        let (status, body) = request(
            &server.address,
            "POST",
            "/v1/databases/pressure/query",
            &owner,
            &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": [chunk]}),
        );
        if status == 500 {
            assert_eq!(body["error"]["code"], "internal_error", "full-disk code: {body}");
            full_at = Some(i);
            break;
        }
        assert_eq!(status, 200, "fill {i}: {body}");
    }
    let filled = full_at.expect("ceiling never tripped in 20 small writes");

    // The failed write rolled back: acknowledged rows are exact — the 3
    // baseline rows plus exactly the fills that were acknowledged.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/pressure/query",
        &owner,
        &json!({"sql": "SELECT id, blob FROM payload ORDER BY id"}),
    );
    assert_eq!(status, 200, "post-pressure read: {body}");
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3 + filled, "no partial write survived");
    let baseline: Vec<Value> =
        json!([[1, "base-0"], [2, "base-1"], [3, "base-2"]]).as_array().unwrap().clone();
    assert_eq!(&rows[..3], &baseline[..], "baseline exact");

    // The pressure is per-file: the control database keeps serving writes.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/control/query",
        &owner,
        &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": ["control-row-2"]}),
    );
    assert_eq!(status, 200, "control write under pressure: {body}");

    // Lifting the ceiling recovers writes: the pressure was the ceiling,
    // not corruption.
    {
        let conn =
            rusqlite::Connection::open(data_dir.path().join("pressure.sqlite")).unwrap();
        conn.execute_batch("PRAGMA max_page_count=1073741823;").unwrap();
    }
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/pressure/query",
        &owner,
        &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": [chunk]}),
    );
    assert_eq!(status, 200, "write after ceiling lifted: {body}");
    assert_eq!(body["rows_affected"], 1);
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/pressure/query",
        &owner,
        &json!({"sql": "SELECT COUNT(*) FROM payload"}),
    );
    assert_eq!(status, 200, "final count: {body}");
    assert_eq!(body["rows"], json!([[3 + filled + 1]]));
}
