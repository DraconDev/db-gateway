//! Which statement is the expensive one: `/v1/queries` and `dg queries`.
//!
//! Run: `timeout 300 cargo test --locked --test query_visibility`
//!
//! Proves the thing a hosted provider cannot do: the gateway knows what
//! each statement cost, and it knows it without keeping the data. A
//! statement is recorded as its SHAPE — the SQL with every literal
//! replaced by a placeholder — so the log answers "how much does this
//! cost" without becoming a second copy of the data. The endpoint is
//! owner-only, because the shapes are a view of every credential's
//! traffic.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
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

fn start(data_dir: &std::path::Path) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dg"));
    cmd.args([
        "serve",
        "--listen",
        "127.0.0.1:0",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
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
    let response = req.timeout(Duration::from_secs(60)).send().unwrap();
    let status = response.status().as_u16();
    let body = response.json().unwrap_or(Value::Null);
    (status, body)
}

fn delete(address: &str, path: &str, cred: &str) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .delete(format!("http://{address}{path}"))
        .bearer_auth(cred)
        .timeout(Duration::from_secs(60))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().unwrap_or(Value::Null);
    (status, body)
}

fn post(address: &str, path: &str, cred: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(cred)
        .json(body)
        .timeout(Duration::from_secs(60))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().unwrap_or(Value::Null);
    (status, body)
}

/// The shapes recorded for one database.
fn shapes(body: &Value, database: &str) -> Vec<Value> {
    body["databases"]
        .as_array()
        .expect("the endpoint returns a list of databases")
        .iter()
        .filter(|entry| entry["database"] == json!(database))
        .flat_map(|entry| entry["shapes"].as_array().cloned().unwrap_or_default())
        .collect()
}

#[test]
fn a_statement_is_recorded_as_its_shape_and_never_as_its_values() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "qdb"}),
    );
    assert_eq!(status, 200);
    let (status, _) = post(
        &server.address,
        "/v1/databases/qdb/query",
        &owner,
        &json!({"sql": "CREATE TABLE t (id INTEGER, email TEXT)", "args": []}),
    );
    assert_eq!(status, 200);
    let (status, _) = post(
        &server.address,
        "/v1/databases/qdb/query",
        &owner,
        &json!({"sql": "INSERT INTO t VALUES (?, ?)", "args": [7, "ada@example.com"]}),
    );
    assert_eq!(status, 200);

    let (status, body) = get(&server.address, "/v1/queries", Some(&owner));
    assert_eq!(status, 200);
    let recorded = shapes(&body, "qdb");
    assert!(
        !recorded.is_empty(),
        "the statements that ran are recorded: {body}"
    );
    let inserted = recorded
        .iter()
        .find(|shape| shape["shape"] == json!("INSERT INTO t VALUES (?, ?)"))
        .expect("the insert is recorded under its shape");
    assert_eq!(inserted["count"], json!(1));
    assert_eq!(inserted["rows_written"], json!(1));

    // The data itself is nowhere in the answer. This is the whole point
    // of a shape: the record names the statement without quoting what
    // it carried.
    let rendered = body.to_string();
    assert!(
        !rendered.contains("ada@example.com"),
        "a statement's values never reach the query log: {rendered}"
    );
    assert!(
        !rendered.contains("\"7\""),
        "a statement's values never reach the query log: {rendered}"
    );
}

#[test]
fn two_spellings_of_the_same_statement_are_one_shape() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "sdb"}),
    );
    assert_eq!(status, 200);
    let (status, _) = post(
        &server.address,
        "/v1/databases/sdb/query",
        &owner,
        &json!({"sql": "CREATE TABLE t (v INTEGER)", "args": []}),
    );
    assert_eq!(status, 200);
    // The same statement with two different literals, and with the
    // operator's spacing written two ways.
    for sql in [
        "SELECT * FROM t WHERE v = 1",
        "SELECT * FROM t WHERE v = 2",
        "SELECT * FROM t WHERE v=3",
    ] {
        let (status, _) = post(
            &server.address,
            "/v1/databases/sdb/query",
            &owner,
            &json!({"sql": sql, "args": []}),
        );
        assert_eq!(status, 200, "{sql}");
    }

    let (status, body) = get(&server.address, "/v1/queries", Some(&owner));
    assert_eq!(status, 200);
    let selects = shapes(&body, "sdb")
        .into_iter()
        .filter(|shape| shape["shape"].as_str().unwrap_or("").starts_with("SELECT"))
        .collect::<Vec<_>>();
    assert_eq!(
        selects.len(),
        1,
        "three literals of one statement are one shape: {selects:?}"
    );
    assert_eq!(selects[0]["shape"], json!("SELECT * FROM t WHERE v = ?"));
    assert_eq!(selects[0]["count"], json!(3));
}

#[test]
fn the_shapes_are_bounded_per_database() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "bdb"}),
    );
    assert_eq!(status, 200);
    // More distinct shapes than the cap keeps. The distinctness has to
    // be in the schema: a number is a value, and values are the data.
    let (status, _) = post(
        &server.address,
        "/v1/databases/bdb/query",
        &owner,
        &json!({"sql": "CREATE TABLE t (v INTEGER)", "args": []}),
    );
    assert_eq!(status, 200);
    for index in 0..(dg::store::MAX_SHAPES_PER_DATABASE + 10) {
        let (status, _) = post(
            &server.address,
            "/v1/databases/bdb/query",
            &owner,
            &json!({"sql": format!("SELECT 1 AS c{index}"), "args": []}),
        );
        assert_eq!(status, 200);
    }
    let (status, body) = get(&server.address, "/v1/queries", Some(&owner));
    assert_eq!(status, 200);
    let recorded = shapes(&body, "bdb");
    assert!(
        recorded.len() <= dg::store::MAX_SHAPES_PER_DATABASE,
        "the log is bounded, not a disk-filling history: {}",
        recorded.len()
    );
}

/// A deleted database's shapes are history, not traffic.
///
/// The log lives in memory and it outlives a deletion for as long as the
/// process runs, so it can still hold shapes for databases that are gone.
/// The tab that shows them answers "which statement is the expensive
/// one", and a database that has been deleted has no expensive statement:
/// it was also filling the tab with rows for things that no longer exist.
#[test]
fn a_deleted_database_does_not_appear_in_the_query_log() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    for name in ["live", "gone"] {
        let (status, _) = post(
            &server.address,
            "/v1/databases",
            &owner,
            &json!({"name": name}),
        );
        assert_eq!(status, 200);
        let (status, _) = post(
            &server.address,
            &format!("/v1/databases/{name}/query"),
            &owner,
            &json!({"sql": "CREATE TABLE t (id INTEGER)", "args": []}),
        );
        assert_eq!(status, 200);
    }
    // The database is deleted, the way an operator deletes one.
    let (status, _) = delete(&server.address, "/v1/databases/gone", &owner);
    assert_eq!(status, 200);

    let (status, body) = get(&server.address, "/v1/queries", Some(&owner));
    assert_eq!(status, 200);
    assert!(
        shapes(&body, "gone").is_empty(),
        "the deleted database still shows its shapes: {body}"
    );
    // The live one is still there.
    assert!(
        !shapes(&body, "live").is_empty(),
        "the live database keeps its shapes: {body}"
    );
    // And the deleted one is counted, so it is not hidden by being
    // dropped.
    assert_eq!(body["deleted"], json!(1));
}

#[test]
fn the_endpoint_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let (status, _) = post(
        &server.address,
        "/v1/databases",
        &owner,
        &json!({"name": "adb"}),
    );
    assert_eq!(status, 200);
    let (status, key) = post(
        &server.address,
        "/v1/keys",
        &owner,
        &json!({"name": "qk", "role": "database_read_only", "databases": ["adb"]}),
    );
    assert_eq!(status, 200);
    let app_key = key["key"].as_str().unwrap();

    // The shapes are a view of every credential's traffic, so a scoped
    // key does not get to read them.
    let (status, _) = get(&server.address, "/v1/queries", Some(app_key));
    assert_eq!(status, 403);
    // And an unauthenticated caller certainly does not.
    let (status, _) = get(&server.address, "/v1/queries", None);
    assert_eq!(status, 401);
}

#[test]
fn the_endpoint_can_narrow_to_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    for name in ["one", "two"] {
        let (status, _) = post(
            &server.address,
            "/v1/databases",
            &owner,
            &json!({"name": name}),
        );
        assert_eq!(status, 200);
        let (status, _) = post(
            &server.address,
            &format!("/v1/databases/{name}/query"),
            &owner,
            &json!({"sql": "CREATE TABLE t (v INTEGER)", "args": []}),
        );
        assert_eq!(status, 200);
    }
    let (status, body) = get(&server.address, "/v1/queries?database=one", Some(&owner));
    assert_eq!(status, 200);
    let databases = body["databases"].as_array().unwrap();
    assert_eq!(databases.len(), 1);
    assert_eq!(databases[0]["database"], json!("one"));
    // Without the filter, both databases are there — so the narrowed
    // answer really did narrow, rather than the other one never being
    // recorded.
    let (status, body) = get(&server.address, "/v1/queries", Some(&owner));
    assert_eq!(status, 200);
    let names: Vec<String> = body["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["database"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
}
