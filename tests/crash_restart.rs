//! Crash/restart coverage for an acknowledged write.
//!
//! Run: `timeout 120 cargo test --locked --test crash_restart`
//!
//! A real `dg serve` process receives a write over HTTP, is force-killed
//! after the acknowledged response, then restarts against the same data
//! directory. The acknowledged row must survive exactly once with a clean
//! integrity check. Synchronization is the HTTP acknowledgement plus the
//! restarted server's address log line — no sleeps.
//!
//! Process termination is NOT power-loss testing: the OS still flushes
//! completed syscalls and the disk is intact. This covers gateway restart
//! durability only.

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
    request_keyed(address, method, path, key, None, body)
}

fn request_keyed(
    address: &str,
    method: &str,
    path: &str,
    key: &str,
    idempotency: Option<&str>,
    body: &Value,
) -> (u16, Value) {
    let mut builder = reqwest::blocking::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(key)
        .json(body)
        .timeout(Duration::from_secs(60));
    if let Some(k) = idempotency {
        builder = builder.header("Idempotency-Key", k);
    }
    let response = builder.send().unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn acknowledged_write_survives_exactly_once_across_force_kill_restart() {
    let data_dir = TempDir::new().unwrap();
    let mut server = spawn_on(data_dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases",
        &owner,
        &json!({"name": "ledger"}),
    );
    assert_eq!(status, 200, "create: {body}");

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/ledger/query",
        &owner,
        &json!({"sql": "CREATE TABLE entries (id INTEGER PRIMARY KEY, label TEXT NOT NULL, amount REAL NOT NULL)"}),
    );
    assert_eq!(status, 200, "ddl: {body}");

    // The acknowledgement is the durability barrier: kill only after it.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/ledger/query",
        &owner,
        &json!({"sql": "INSERT INTO entries (label, amount) VALUES (?, ?)", "args": ["opening", 42.5]}),
    );
    assert_eq!(status, 200, "insert: {body}");
    assert_eq!(body["rows_affected"], 1);

    // Force termination: no graceful shutdown handshake.
    server.child.kill().unwrap();
    server.child.wait().unwrap();

    let restarted = spawn_on(data_dir.path());
    let (status, body) = request(
        &restarted.address,
        "POST",
        "/v1/databases/ledger/query",
        &owner,
        &json!({"sql": "SELECT id, label, amount FROM entries"}),
    );
    assert_eq!(status, 200, "post-restart read: {body}");
    assert_eq!(body["columns"], json!(["id", "label", "amount"]));
    assert_eq!(body["rows"], json!([[1, "opening", 42.5]]));

    // File-level integrity on the restarted database.
    let conn = rusqlite::Connection::open(data_dir.path().join("ledger.sqlite")).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
}

#[test]
fn keyed_write_replays_identically_after_restart_without_duplicating() {
    let data_dir = TempDir::new().unwrap();
    let mut server = spawn_on(data_dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases",
        &owner,
        &json!({"name": "orders"}),
    );
    assert_eq!(status, 200, "create: {body}");

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/orders/query",
        &owner,
        &json!({"sql": "CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT NOT NULL)"}),
    );
    assert_eq!(status, 200, "ddl: {body}");

    // Non-idempotent INSERT committed under a real Idempotency-Key header.
    let write = json!({"sql": "INSERT INTO events (kind) VALUES (?)", "args": ["placed"]});
    let (status, first) = request_keyed(
        &server.address,
        "POST",
        "/v1/databases/orders/query",
        &owner,
        Some("replay-key-1"),
        &write,
    );
    assert_eq!(status, 200, "keyed insert: {first}");
    assert_eq!(first["rows_affected"], 1);

    // Restart before the client observes any replay: same directory.
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let restarted = spawn_on(data_dir.path());

    // Identical principal + key + body replays the stored response.
    let (status, second) = request_keyed(
        &restarted.address,
        "POST",
        "/v1/databases/orders/query",
        &owner,
        Some("replay-key-1"),
        &write,
    );
    assert_eq!(status, 200, "replay: {second}");
    assert_eq!(second, first);

    // Exactly one row: the replay never re-executed the INSERT.
    let (status, body) = request(
        &restarted.address,
        "POST",
        "/v1/databases/orders/query",
        &owner,
        &json!({"sql": "SELECT id, kind FROM events"}),
    );
    assert_eq!(status, 200, "post-replay read: {body}");
    assert_eq!(body["rows"], json!([[1, "placed"]]));
}
