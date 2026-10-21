//! Ten-round kill -9 mid-write rough ride (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test kill_midwrite_10x`
//!
//! Each round starts a real `dg serve` on a shared data dir, fires one
//! atomic 32-row dummy batch with a per-round `Idempotency-Key`, then
//! SIGKILLs the server after a short varied delay and moves on without
//! waiting for the response. After all ten rounds a final server starts
//! and asserts every round left either 0 or 32 rows (never a partial
//! 1..31), the total is exact, and `PRAGMA integrity_check` is `ok`.
//! Readiness is the server's logged listen address — no sleep-based sync
//! except the deliberate kill delays, which only vary *when* the kill
//! lands, never whether the assertions hold.

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

fn request(address: &str, path: &str, key: &str, idempotency: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(key)
        .header("Idempotency-Key", idempotency)
        .json(body)
        .timeout(Duration::from_secs(60))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

/// Fire-and-forget: the kill may land before any response exists, so every
/// transport failure is a legal outcome, never a panic.
fn fire_unsupervised(address: String, path: String, key: String, idempotency: String, body: Value) {
    let _ = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(key)
        .header("Idempotency-Key", idempotency)
        .json(&body)
        .timeout(Duration::from_secs(60))
        .send();
}

fn count_where(address: &str, key: &str, like: &str) -> i64 {
    let (status, body) = request(
        address,
        "/v1/databases/kill/query",
        key,
        &format!("count-{like}"),
        &json!({"sql": "SELECT count(*) FROM k WHERE tag LIKE ?", "args": [like]}),
    );
    assert_eq!(status, 200, "count {like}: {body}");
    body["rows"][0][0].as_i64().unwrap()
}

#[test]
fn ten_sigkill_mid_write_rounds_leave_no_partial_batches() {
    let data_dir = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    let setup = spawn_on(data_dir.path());
    let (status, body) = request(
        &setup.address,
        "/v1/databases",
        &owner,
        "setup-create",
        &json!({"name": "kill"}),
    );
    assert_eq!(status, 200, "create: {body}");
    let (status, body) = request(
        &setup.address,
        "/v1/databases/kill/query",
        &owner,
        "setup-ddl",
        &json!({"sql": "CREATE TABLE k(tag TEXT PRIMARY KEY, n INTEGER NOT NULL)"}),
    );
    assert_eq!(status, 200, "ddl: {body}");
    // Park the setup server; each round below spawns its own.
    drop(setup);

    // Varied kill delays (ms): only the kill timing varies, the 0-or-32
    // invariant must hold for every round regardless of when it lands.
    let delays = [5, 15, 25, 35, 45, 55, 65, 75, 85, 95];
    for (i, delay_ms) in delays.iter().enumerate() {
        let mut server = spawn_on(data_dir.path());
        let statements: Vec<Value> = (0..32)
            .map(|j| json!({"sql": format!("INSERT INTO k(tag, n) VALUES ('r{i}-{j}', {j})")}))
            .collect();
        let batch = json!({"statements": statements, "atomic": true});
        let (address, key) = (server.address.clone(), owner.clone());
        let flight = std::thread::spawn(move || {
            fire_unsupervised(
                address,
                "/v1/databases/kill/batch".to_string(),
                key,
                format!("kill-key-{i}"),
                batch,
            )
        });
        std::thread::sleep(Duration::from_millis(*delay_ms));
        // SIGKILL on Unix: no graceful shutdown handshake.
        server.child.kill().unwrap();
        server.child.wait().unwrap();
        let _ = flight.join();
    }

    let final_server = spawn_on(data_dir.path());
    let mut applied = 0;
    for i in 0..10 {
        let n = count_where(&final_server.address, &owner, &format!("r{i}-%"));
        assert!(
            n == 0 || n == 32,
            "round {i} left a partial batch: {n} rows (must be 0 or 32)"
        );
        if n == 32 {
            applied += 1;
        }
    }
    let (status, body) = request(
        &final_server.address,
        "/v1/databases/kill/query",
        &owner,
        "final-total",
        &json!({"sql": "SELECT count(*), count(DISTINCT tag) FROM k"}),
    );
    assert_eq!(status, 200, "total: {body}");
    assert_eq!(body["rows"], json!([[applied * 32, applied * 32]]));

    // File-level integrity on the restarted database.
    let conn = rusqlite::Connection::open(data_dir.path().join("kill.sqlite")).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
}
