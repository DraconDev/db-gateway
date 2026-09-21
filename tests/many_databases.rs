//! Many-database scale: hundreds of persisted databases on one host.
//!
//! Run: `timeout 300 cargo test --locked --test many_databases`
//!
//! Proves the "hundreds of DBs" requirement against a real `dg serve`:
//! 300 databases are created, each receives a unique row, the server is
//! restarted, and every row plus the full listing must survive exactly.
//! A store-level test pins the registry cap above the requirement.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};

/// The scale requirement: hundreds of databases, not dozens.
const DB_COUNT: usize = 300;

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

fn start(data_dir: &std::path::Path) -> Server {
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
    // Drain stderr in the background so a chatty server never blocks.
    Server {
        child,
        address: address.expect("server logged its listen address"),
    }
}

fn req(
    client: &reqwest::blocking::Client,
    address: &str,
    method: &str,
    path: &str,
    cred: &str,
    body: &Value,
) -> (u16, Value) {
    let response = client
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(cred)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

#[test]
fn three_hundred_databases_persist_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let client = reqwest::blocking::Client::new();
    let name = |i: usize| format!("db{i:03}");

    for i in 0..DB_COUNT {
        let db = name(i);
        let (status, _) = req(
            &client,
            &server.address,
            "POST",
            "/v1/databases",
            &owner,
            &json!({"name": db}),
        );
        assert_eq!(status, 200, "create {db}");
        // One atomic batch per database: schema plus its unique row.
        let (status, _) = req(
            &client,
            &server.address,
            "POST",
            &format!("/v1/databases/{db}/batch"),
            &owner,
            &json!({
                "atomic": true,
                "statements": [
                    {"sql": "CREATE TABLE t (v TEXT)", "args": []},
                    {"sql": "INSERT INTO t VALUES (?)", "args": [db]},
                ],
            }),
        );
        assert_eq!(status, 200, "seed {db}");
    }

    let (status, list) = req(
        &client,
        &server.address,
        "GET",
        "/v1/databases",
        &owner,
        &json!(null),
    );
    assert_eq!(status, 200);
    assert_eq!(list["databases"].as_array().unwrap().len(), DB_COUNT);

    // Restart against the same data dir: everything must survive exactly.
    drop(server);
    let server = start(dir.path());
    let (status, list) = req(
        &client,
        &server.address,
        "GET",
        "/v1/databases",
        &owner,
        &json!(null),
    );
    assert_eq!(status, 200);
    assert_eq!(list["databases"].as_array().unwrap().len(), DB_COUNT);
    for i in 0..DB_COUNT {
        let db = name(i);
        let (status, rows) = req(
            &client,
            &server.address,
            "POST",
            &format!("/v1/databases/{db}/query"),
            &owner,
            &json!({"sql": "SELECT v FROM t", "args": []}),
        );
        assert_eq!(status, 200, "read {db} after restart");
        assert_eq!(rows["rows"], json!([[db]]), "exact row {db}");
    }
}

#[test]
fn registry_cap_supports_hundreds() {
    assert!(
        dg::store::MAX_DATABASES >= DB_COUNT as i64,
        "registry cap must cover the hundreds-of-DBs requirement"
    );
    let dir = tempfile::tempdir().unwrap();
    let store = dg::store::Store::open(dir.path()).unwrap();
    store.bootstrap().unwrap();
    for i in 0..dg::store::MAX_DATABASES {
        store.create(&format!("cap{i:04}")).unwrap();
    }
    // Both publication paths share the cap and report it as backpressure.
    assert_eq!(
        store.create("one_too_many").unwrap_err().code,
        "capacity_exceeded"
    );
    assert_eq!(
        store
            .import_from("import_at_cap", std::path::Path::new("/nonexistent"))
            .unwrap_err()
            .code,
        "capacity_exceeded"
    );
}
