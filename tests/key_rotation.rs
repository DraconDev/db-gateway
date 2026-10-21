//! Owner key rotation (dummy data).
//!
//! Run: `timeout 120 cargo test --locked --test key_rotation`
//!
//! `POST /v1/keys/owner/rotate` replaces the owner credential atomically:
//! the old key 401s immediately, the new key manages, databases and app-key
//! scopes are untouched, and a second rotation works. The CLI covers the same
//! flow through `dg key rotate-owner` with real 0600 key files.

#[path = "common/mod.rs"]
mod common;

use std::{
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::Duration,
};

use common::{Cli, Fleet};
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

fn spawn(data_dir: &std::path::Path) -> (Server, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
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
    (
        Server {
            child,
            address: address.expect("server logged its listen address"),
        },
        owner,
    )
}

fn req(
    client: &reqwest::blocking::Client,
    address: &str,
    method: &str,
    path: &str,
    cred: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut request = client
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(cred)
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

fn is_hex64(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())
}

#[test]
fn rotation_kills_old_key_keeps_data_and_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = spawn(dir.path());
    let a = &server.address;
    let client = reqwest::blocking::Client::new();

    let (status, _) = req(
        &client,
        a,
        "POST",
        "/v1/databases",
        &owner,
        Some(&json!({"name": "shop"})),
    );
    assert_eq!(status, 200);
    let (status, _) = req(
        &client,
        a,
        "POST",
        "/v1/databases/shop/query",
        &owner,
        Some(&json!({"sql": "CREATE TABLE t (v TEXT)", "args": []})),
    );
    assert_eq!(status, 200);
    let (status, _) = req(
        &client,
        a,
        "POST",
        "/v1/databases/shop/query",
        &owner,
        Some(&json!({"sql": "INSERT INTO t VALUES ('keep')", "args": []})),
    );
    assert_eq!(status, 200);
    let (status, body) = req(
        &client,
        a,
        "POST",
        "/v1/keys",
        &owner,
        Some(&json!({"name": "app", "role": "database_read_write", "databases": ["shop"]})),
    );
    assert_eq!(status, 200);
    let app = body["key"].as_str().unwrap().to_string();

    // App keys cannot rotate.
    let (status, body) = req(&client, a, "POST", "/v1/keys/owner/rotate", &app, None);
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "forbidden");

    let (status, body) = req(&client, a, "POST", "/v1/keys/owner/rotate", &owner, None);
    assert_eq!(status, 200);
    let rotated = body["key"].as_str().unwrap().to_string();
    assert!(is_hex64(&rotated) && rotated != owner);
    assert_eq!(
        body.as_object().unwrap().len(),
        1,
        "key response carries no metadata"
    );

    // Old owner key is dead everywhere immediately.
    for (method, path, body) in [
        ("GET", "/v1/databases", None),
        (
            "POST",
            "/v1/databases/shop/query",
            Some(json!({"sql": "SELECT 1"})),
        ),
        ("POST", "/v1/keys/owner/rotate", None),
    ] {
        let (status, body) = req(&client, a, method, path, &owner, body.as_ref());
        assert_eq!(status, 401, "old key on {method} {path}");
        assert_eq!(body["error"]["code"], "invalid_credential");
    }

    // New owner key manages; data and app scopes survived.
    let (status, body) = req(&client, a, "GET", "/v1/databases", &rotated, None);
    assert_eq!(status, 200);
    assert_eq!(body["databases"], json!(["shop"]));
    let (status, body) = req(
        &client,
        a,
        "POST",
        "/v1/databases/shop/query",
        &app,
        Some(&json!({"sql": "SELECT v FROM t", "args": []})),
    );
    assert_eq!(status, 200);
    assert_eq!(body["rows"], json!([["keep"]]));
    let (status, _) = req(
        &client,
        a,
        "POST",
        "/v1/databases",
        &rotated,
        Some(&json!({"name": "after"})),
    );
    assert_eq!(status, 200, "rotated key creates");

    // A second rotation retires the first rotated key the same way.
    let (status, body) = req(&client, a, "POST", "/v1/keys/owner/rotate", &rotated, None);
    assert_eq!(status, 200);
    let twice = body["key"].as_str().unwrap().to_string();
    assert!(is_hex64(&twice) && twice != rotated);
    let (status, _) = req(&client, a, "GET", "/v1/databases", &rotated, None);
    assert_eq!(status, 401);
    let (status, body) = req(&client, a, "GET", "/v1/databases", &twice, None);
    assert_eq!(status, 200);
    assert_eq!(body["databases"], json!(["after", "shop"]));
}

#[test]
fn cli_rotate_owner_end_to_end() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "shop"]);
    fleet
        .cli
        .ok("seed", &["query", "shop", "CREATE TABLE t (v TEXT)"]);
    fleet.cli.ok(
        "insert",
        &["query", "shop", "INSERT INTO t VALUES ('keep')"],
    );

    let rotated = fleet.cli.ok("rotate", &["key", "rotate-owner"]);
    let key = rotated["key"].as_str().unwrap().to_string();
    assert!(is_hex64(&key));
    let dir = tempfile::tempdir().unwrap();
    let new_file = dir.path().join("new.key");
    std::fs::write(&new_file, format!("{key}\n")).unwrap();
    std::fs::set_permissions(&new_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let fresh = Cli {
        server: fleet.cli.server.clone(),
        key_file: new_file,
    };

    // Old key file is dead; new key file manages and reads exact rows.
    let (success, stdout, stderr) = fleet.cli.run(&["list"]);
    assert!(!success, "old key still works: {stdout} {stderr}");
    assert!(stdout.contains("invalid_credential") || stderr.contains("invalid_credential"));
    assert_eq!(
        fresh.ok("list", &["list"])["databases"],
        json!(["shop"]),
        "rotated key lists"
    );
    assert_eq!(
        fresh.q("read", "shop", "SELECT v FROM t"),
        json!([["keep"]]),
        "data survives rotation"
    );
}
