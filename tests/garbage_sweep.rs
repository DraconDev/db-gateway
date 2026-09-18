//! Oversize and garbage input sweep (dummy data).
//!
//! Run: `timeout 120 cargo test --locked --test garbage_sweep`
//!
//! A real `dg serve` receives malformed, oversized, and mistyped request
//! bodies over HTTP. Every case must be rejected with a stable 4xx code
//! and a JSON error body, and a follow-up valid write plus read must
//! succeed with exact rows — proving the server stayed up and uncorrupted.
//! Statuses below pin observed behavior; the NUL-byte case documents that
//! SQLite truncates at the NUL (`SELECT 1` executes) rather than erroring.

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

/// Raw-bytes POST: bypasses JSON serialization so malformed and non-UTF8
/// bodies reach the server exactly as written.
fn post_raw(address: &str, path: &str, key: &str, body: Vec<u8>) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(key)
        .header("Content-Type", "application/json")
        .body(body)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

fn post_json(address: &str, path: &str, key: &str, body: &Value) -> (u16, Value) {
    post_raw(address, path, key, serde_json::to_vec(body).unwrap())
}

#[test]
fn garbage_inputs_reject_cleanly_and_server_recovers() {
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

    let server = spawn_on(data_dir.path());
    let q = "/v1/databases/junk/query";
    let b = "/v1/databases/junk/batch";

    let (status, body) = post_json(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "junk"}),
    );
    assert_eq!(status, 200, "create: {body}");
    let (status, body) = post_json(
        &server.address,
        q,
        &owner,
        &json!({"sql": "CREATE TABLE t(a INTEGER NOT NULL)"}),
    );
    assert_eq!(status, 200, "ddl: {body}");

    // (name, path, raw body, bearer override, expected status)
    let big_sql = format!("SELECT 1 --{}", "x".repeat(70_000));
    let many: Vec<Value> = (0..33).map(|_| json!({"sql": "SELECT 1"})).collect();
    let args257: Vec<Value> = (0..257).map(|i| json!(i)).collect();
    let bad_key = "0".repeat(64);
    let cases: Vec<(&str, &str, Vec<u8>, Option<&str>, u16)> = vec![
        (
            "oversize_sql_70k",
            q,
            serde_json::to_vec(&json!({"sql": big_sql})).unwrap(),
            None,
            413,
        ),
        (
            "batch_33",
            b,
            serde_json::to_vec(&json!({"statements": many, "atomic": true})).unwrap(),
            None,
            400,
        ),
        (
            "batch_empty",
            b,
            serde_json::to_vec(&json!({"statements": [], "atomic": true})).unwrap(),
            None,
            400,
        ),
        (
            "batch_nonatomic",
            b,
            serde_json::to_vec(&json!({"statements": [{"sql": "SELECT 1"}], "atomic": false}))
                .unwrap(),
            None,
            400,
        ),
        ("malformed_json", q, b"{not json".to_vec(), None, 400),
        ("empty_body", q, vec![], None, 400),
        (
            "non_utf8",
            q,
            b"\xff\xfe\x00\x01binary-junk".to_vec(),
            None,
            400,
        ),
        ("wrong_shape", q, br#"{"sql": 123}"#.to_vec(), None, 400),
        (
            "unknown_field",
            q,
            br#"{"sql": "SELECT 1", "bogus": true}"#.to_vec(),
            None,
            400,
        ),
        (
            "args_257",
            q,
            serde_json::to_vec(
                &json!({"sql": format!("SELECT {}", vec!["?"; 257].join("+")), "args": args257}),
            )
            .unwrap(),
            None,
            400,
        ),
        ("null_body", q, b"null".to_vec(), None, 400),
        ("array_body", q, b"[1,2]".to_vec(), None, 400),
        (
            "bad_bearer",
            q,
            br#"{"sql": "SELECT 1"}"#.to_vec(),
            Some(bad_key.as_str()),
            401,
        ),
    ];
    for (name, path, raw, bearer, want) in &cases {
        let (status, body) = post_raw(&server.address, path, bearer.unwrap_or(&owner), raw.clone());
        assert_eq!(status, *want, "{name}: {body}");
        assert!(
            body["error"]["code"].is_string(),
            "{name}: no error code: {body}"
        );
    }

    // NUL byte: SQLite truncates at the NUL, so `SELECT 1` executes.
    // Pinned as observed behavior plus proof of no corruption after.
    let (status, body) = post_json(
        &server.address,
        q,
        &owner,
        &json!({"sql": "SELECT 1\u{0000}"}),
    );
    assert_eq!(status, 200, "nul: {body}");
    assert_eq!(body["rows"], json!([[1]]));

    // Follow-up valid write and read: exact rows, server healthy.
    let (status, body) = post_json(
        &server.address,
        q,
        &owner,
        &json!({"sql": "INSERT INTO t(a) VALUES (?)", "args": [7]}),
    );
    assert_eq!(status, 200, "follow-up write: {body}");
    assert_eq!(body["rows_affected"], 1);
    let (status, body) = post_json(
        &server.address,
        q,
        &owner,
        &json!({"sql": "SELECT a FROM t"}),
    );
    assert_eq!(status, 200, "follow-up read: {body}");
    assert_eq!(body["rows"], json!([[7]]));
}
