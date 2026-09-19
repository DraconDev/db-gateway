//! Import/export round-trip torture (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test import_export_torture`
//!
//! A real `dg serve` holds one dummy source database with mixed-type rows.
//! Twenty cycles each export it, then either import the bytes intact under
//! a fresh name (rows must match the source exactly) or corrupt a byte and
//! import (must fail with no publish: the name stays unqueryable and no
//! file appears in the data dir). The finale asserts the management list
//! route shows exactly the source plus the twenty good copies — the
//! registry is clean of corrupt-import residue.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as B64};
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

fn post(address: &str, method: &str, path: &str, key: &str, body: &Value) -> (u16, Value) {
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

fn query(address: &str, key: &str, db: &str, sql: &str) -> (u16, Value) {
    post(
        address,
        "POST",
        &format!("/v1/databases/{db}/query"),
        key,
        &json!({"sql": sql}),
    )
}

#[test]
fn twenty_export_import_cycles_with_corruption() {
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
    let (status, body) = post(
        &server.address,
        "POST",
        "/v1/databases",
        &owner,
        &json!({"name": "src"}),
    );
    assert_eq!(status, 200, "create: {body}");

    // Mixed-type dummy source: INTEGER/REAL/TEXT/BLOB/NULL plus a row id.
    let (status, body) = post(
        &server.address,
        "POST",
        "/v1/databases/src/batch",
        &owner,
        &json!({"statements": [
            {"sql": "CREATE TABLE d(id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, payload BLOB, note TEXT)"},
            {"sql": "INSERT INTO d(name, score, payload, note) VALUES ('alpha', 1.5, randomblob(32), NULL)"},
            {"sql": "INSERT INTO d(name, score, payload, note) VALUES ('beta', -2.25, zeroblob(16), 'has-note')"},
            {"sql": "INSERT INTO d(name, score, payload, note) VALUES ('gamma', 0.0, x'DEADBEEF', '')"},
        ], "atomic": true}),
    );
    assert_eq!(status, 200, "seed: {body}");
    let (status, source) = query(
        &server.address,
        &owner,
        "src",
        "SELECT id, name, score, hex(payload), note FROM d ORDER BY id",
    );
    assert_eq!(status, 200, "source read: {source}");
    assert_eq!(source["rows"].as_array().unwrap().len(), 3);
    assert_eq!(source["rows"][0][1], json!("alpha"));
    assert_eq!(
        source["rows"][1],
        json!([
            2,
            "beta",
            -2.25,
            "00000000000000000000000000000000",
            "has-note"
        ])
    );
    assert_eq!(source["rows"][2][1], json!("gamma"));
    assert_eq!(source["rows"][2][3], json!("DEADBEEF"));
    assert_eq!(source["rows"][2][0], json!(3));

    for i in 0..20 {
        let (status, exp) = post(
            &server.address,
            "POST",
            "/v1/databases/src/export",
            &owner,
            &json!({}),
        );
        assert_eq!(status, 200, "cycle {i} export: {exp}");
        let bytes = B64.decode(exp["data_base64"].as_str().unwrap()).unwrap();
        assert!(!bytes.is_empty(), "cycle {i}: empty export");

        if i % 2 == 0 {
            // Good path: intact bytes import under a fresh name, exact rows.
            let name = format!("rt_good_{i}");
            let (status, imp) = post(
                &server.address,
                "POST",
                &format!("/v1/databases/{name}/import"),
                &owner,
                &json!({"name": name, "data_base64": B64.encode(&bytes)}),
            );
            assert_eq!(status, 200, "cycle {i} import: {imp}");
            let (status, rows) = query(
                &server.address,
                &owner,
                &name,
                "SELECT id, name, score, hex(payload), note FROM d ORDER BY id",
            );
            assert_eq!(status, 200, "cycle {i} read: {rows}");
            assert_eq!(
                rows["rows"], source["rows"],
                "cycle {i}: copy differs from source"
            );
        } else {
            // Corrupt path: header-magic flip on some rounds, severe
            // truncation on others. Both must be rejected with no publish.
            let mut bad = bytes.clone();
            if i % 4 == 1 {
                bad[0] ^= 0xFF;
            } else {
                bad.truncate(37);
            }
            let name = format!("rt_bad_{i}");
            let (status, imp) = post(
                &server.address,
                "POST",
                &format!("/v1/databases/{name}/import"),
                &owner,
                &json!({"name": name, "data_base64": B64.encode(&bad)}),
            );
            assert_ne!(status, 200, "cycle {i}: corrupt import accepted: {imp}");
            let (status, q) = query(&server.address, &owner, &name, "SELECT 1");
            assert_eq!(status, 404, "cycle {i}: corrupt name queryable: {q}");
            assert_eq!(q["error"]["code"], "database_not_found");
            assert!(
                !data_dir.path().join(format!("{name}.sqlite")).exists(),
                "cycle {i}: corrupt import published a file"
            );
        }
    }

    // Registry clean: source plus exactly the ten good copies, nothing else.
    let response = reqwest::blocking::Client::new()
        .get(format!("http://{}/v1/databases", server.address))
        .bearer_auth(&owner)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let list: Value = response.json().unwrap();
    let mut names: Vec<String> = list["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    names.sort();
    let mut want: Vec<String> = (0..20).step_by(2).map(|i| format!("rt_good_{i}")).collect();
    want.push("src".to_string());
    want.sort();
    assert_eq!(names, want, "registry residue: {list}");
}
