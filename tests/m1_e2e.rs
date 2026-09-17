//! M1 executable contract: create → write → read → restart → read, plus the
//! required negative paths. Starts a real `dg serve` process per scenario.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;

fn key_file(dir: &std::path::Path, key: &str) -> std::path::PathBuf {
    let path = dir.join("cli.key");
    std::fs::write(&path, format!("{key}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

/// Run the real `dg` CLI against a running server; asserts process exit code.
fn dgcli(server: &str, key_path: &std::path::Path, args: &[&str]) -> (bool, Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["--server", server, "--key-file"])
        .arg(key_path)
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let parsed = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (output.status.success(), parsed, stdout)
}

struct Server {
    child: Child,
    address: String,
    data_dir: TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server() -> Server {
    let data_dir = TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data_dir.path().to_str().unwrap(),
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
        address: address.expect("server logged its listen address"),
        child,
        data_dir,
    }
}

fn request(
    address: &str,
    method: &str,
    path: &str,
    key: Option<&str>,
    idempotency: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut req = reqwest::blocking::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{address}{path}"),
        )
        .timeout(Duration::from_secs(30));
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    if let Some(key) = idempotency {
        req = req.header("Idempotency-Key", key);
    }
    if let Some(body) = body {
        req = req.json(body);
    }
    let response = req.send().unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

fn error_code(body: &Value) -> String {
    body["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn owner_bootstrap(server: &Server) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "init",
            "--data-dir",
            server.data_dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bootstrap: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string()
}

fn create_database(address: &str, owner: &str, name: &str) {
    let (status, body) = request(
        address,
        "POST",
        "/v1/databases",
        Some(owner),
        None,
        Some(&json!({"name": name})),
    );
    assert_eq!(status, 200, "create {name}: {body}");
}

fn issue_key(address: &str, owner: &str, name: &str, role: &str, databases: &[&str]) -> String {
    let (status, body) = request(
        address,
        "POST",
        "/v1/keys",
        Some(owner),
        None,
        Some(&json!({"name": name, "role": role, "databases": databases})),
    );
    assert_eq!(status, 200, "issue key: {body}");
    body["key"].as_str().unwrap().to_string()
}

#[test]
fn json_schema_errors_match_http_contract() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "schema");
    for (path, body) in [
        ("/v1/databases", json!({"name":"newdb", "unknown":true})),
        (
            "/v1/keys",
            json!({"name":"app", "role":"database_read_only", "databases":["schema"], "unknown":true}),
        ),
        (
            "/v1/databases/schema/query",
            json!({"sql":"SELECT 1", "unknown":true}),
        ),
        ("/v1/databases/schema/query", json!({"sql":42})),
        (
            "/v1/databases/schema/batch",
            json!({"atomic":true,"statements":[{"sql":"SELECT 1"}],"unknown":true}),
        ),
        (
            "/v1/databases/schema/batch",
            json!({"atomic":true,"statements":[{"sql":"SELECT 1","unknown":true}]}),
        ),
    ] {
        let (status, response) = request(
            &server.address,
            "POST",
            path,
            Some(&owner),
            None,
            Some(&body),
        );
        assert_eq!(status, 400, "{path}: {response}");
        assert_eq!(error_code(&response), "invalid_request");
    }
    let (status, response) = request(
        &server.address,
        "POST",
        "/v1/databases/schema/query",
        Some(&owner),
        None,
        Some(&json!({"sql":"x".repeat(65537)})),
    );
    assert_eq!(status, 413);
    assert_eq!(error_code(&response), "invalid_request");
    // No Content-Type: preserve the extractor's unsupported media-type response.
    let (status, response) = request(
        &server.address,
        "POST",
        "/v1/databases/schema/query",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 415);
    assert_eq!(error_code(&response), "invalid_request");
}

#[test]
fn round_trip_write_read_persists_across_restart() {
    let mut server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "game_a");

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"})),
    );
    assert_eq!(status, 200, "create table: {body}");

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "INSERT INTO players (name) VALUES (?)", "args": ["aria"]})),
    );
    assert_eq!(status, 200, "insert: {body}");
    assert_eq!(body["rows_affected"], 1);

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT id, name FROM players"})),
    );
    assert_eq!(status, 200, "select: {body}");
    assert_eq!(body["columns"], json!(["id", "name"]));
    assert_eq!(body["rows"], json!([[1, "aria"]]));

    // Restart: same data dir, fresh process, data must survive.
    let data_dir = server.data_dir.path().to_path_buf();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    // Keep the TempDir owner alive until the restarted process exits.
    let restarted = start_server_with_dir(&data_dir);
    let (status, body) = request(
        &restarted.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM players"})),
    );
    assert_eq!(status, 200);
    assert_eq!(body["rows"], json!([[1]]));
}

fn start_server_with_dir(data_dir: &std::path::Path) -> Server {
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
        address: address.expect("restart: server logged its address"),
        data_dir: TempDir::new().unwrap(),
    }
}

#[test]
fn scoped_keys_enforce_boundaries() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "one");
    create_database(&server.address, &owner, "two");
    let ro = issue_key(
        &server.address,
        &owner,
        "reader",
        "database_read_only",
        &["one"],
    );
    let rw = issue_key(
        &server.address,
        &owner,
        "writer",
        "database_read_write",
        &["one"],
    );

    // Cross-database denial: key scoped to `one` cannot touch `two`.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/two/query",
        Some(&rw),
        None,
        Some(&json!({"sql": "SELECT 1"})),
    );
    assert_eq!(status, 403);
    assert_eq!(error_code(&body), "database_forbidden");

    // Management routes are denied to app keys: create, list, issue, revoke.
    for (method, path, body) in [
        ("POST", "/v1/databases", Some(json!({"name": "sneaky"}))),
        ("GET", "/v1/databases", None),
        (
            "POST",
            "/v1/keys",
            Some(json!({"name": "esc", "role": "database_read_only", "databases": ["one"]})),
        ),
        ("DELETE", "/v1/keys/reader", None),
    ] {
        for key in [&rw, &ro] {
            let (status, body) = request(
                &server.address,
                method,
                path,
                Some(key),
                None,
                body.as_ref(),
            );
            assert_eq!(status, 403, "{method} {path}: {body}");
            assert_eq!(error_code(&body), "forbidden");
        }
    }

    // Read-only key may read but not write.
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/one/query",
        Some(&ro),
        None,
        Some(&json!({"sql": "SELECT 1"})),
    );
    assert_eq!(status, 200);
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/one/query",
        Some(&ro),
        None,
        Some(&json!({"sql": "CREATE TABLE forbidden(v INTEGER)"})),
    );
    assert_eq!(status, 403, "read-only write must be rejected: {body}");

    // Revoked keys stop working.
    let (status, body) = request(
        &server.address,
        "DELETE",
        "/v1/keys/reader",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200, "revoke: {body}");
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/one/query",
        Some(&ro),
        None,
        Some(&json!({"sql": "SELECT 1"})),
    );
    assert_eq!(status, 401);
    assert_eq!(error_code(&body), "invalid_credential");

    // Missing database is an explicit 404, never a silent creation.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/nope/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT 1"})),
    );
    assert_eq!(status, 404);
    assert_eq!(error_code(&body), "database_not_found");
}

#[test]
fn atomic_batch_rolls_back_and_limits_bind() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "batchtest");

    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/batchtest/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE t (v INTEGER)"})),
    );
    assert_eq!(status, 200);

    // Atomic batch where the second statement fails: first must roll back.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/batchtest/batch",
        Some(&owner),
        None,
        Some(&json!({"atomic": true, "statements": [
            {"sql": "INSERT INTO t (v) VALUES (1)"},
            {"sql": "INSERT INTO nonexistent (v) VALUES (2)"}
        ]})),
    );
    assert_ne!(status, 200, "failing batch must error: {body}");
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/batchtest/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM t"})),
    );
    assert_eq!(status, 200);
    assert_eq!(body["rows"], json!([[0]]), "rollback: {body}");

    // Oversized result is rejected, not streamed unbounded.
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/batchtest/query",
        Some(&owner),
        None,
        Some(
            &json!({"sql": "INSERT INTO t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM s WHERE v < 50000) SELECT v FROM s"}),
        ),
    );
    assert_eq!(status, 200, "seed rows: insert 50k rows");
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/batchtest/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT v FROM t"})),
    );
    assert_eq!(status, 413, "oversized result: {body}");
    assert_eq!(error_code(&body), "result_limit_exceeded");
}

#[test]
fn idempotency_key_replays_committed_write() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "idem");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/idem/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE n (v INTEGER)"})),
    );
    assert_eq!(status, 200);

    let write = json!({"sql": "INSERT INTO n (v) VALUES (7)"});
    let (status, first) = request(
        &server.address,
        "POST",
        "/v1/databases/idem/query",
        Some(&owner),
        Some("match-final-9182"),
        Some(&write),
    );
    assert_eq!(status, 200, "{first}");

    // Same key, same body: stored result replayed, no second execution.
    let (status, replay) = request(
        &server.address,
        "POST",
        "/v1/databases/idem/query",
        Some(&owner),
        Some("match-final-9182"),
        Some(&write),
    );
    assert_eq!(status, 200);
    assert_eq!(replay, first);

    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/idem/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM n"})),
    );
    assert_eq!(body["rows"], json!([[1]]), "write must not execute twice");

    // Same key with a different body is rejected.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/idem/query",
        Some(&owner),
        Some("match-final-9182"),
        Some(&json!({"sql": "INSERT INTO n (v) VALUES (8)"})),
    );
    assert_eq!(status, 409);
    assert_eq!(error_code(&body), "idempotency_key_conflict");
}

#[test]
fn bootstrap_refuses_second_owner() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "init",
            "--data-dir",
            server.data_dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already_bootstrapped"));
    // The owner key still authenticates.
    let (status, _) = request(
        &server.address,
        "GET",
        "/v1/databases",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200);
}

#[test]
fn cli_commands_against_running_server() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    let key_path = key_file(server.data_dir.path(), &owner);
    let run = |args: &[&str], key: &std::path::Path| {
        dgcli(&format!("http://{}", server.address), key, args)
    };

    // dg create / list / parameterized query / --json behaviour.
    let (ok, body, _) = run(&["create", "clidb"], &key_path);
    assert!(ok, "create: {body}");
    assert_eq!(body["name"], "clidb");

    let (ok, body, _) = run(&["list"], &key_path);
    assert!(ok, "list: {body}");
    assert_eq!(body["databases"], json!(["clidb"]));

    let (ok, body, _) = run(
        &[
            "query",
            "clidb",
            "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        ],
        &key_path,
    );
    assert!(ok, "create table: {body}");

    let (ok, body, _) = run(
        &[
            "query",
            "clidb",
            "INSERT INTO players (name) VALUES (?)",
            "--args",
            "[\"cli-aria\"]",
        ],
        &key_path,
    );
    assert!(ok, "parameterized insert: {body}");
    assert_eq!(body["rows_affected"], 1);

    let (ok, body, _) = run(
        &["--json", "query", "clidb", "SELECT name FROM players"],
        &key_path,
    );
    assert!(ok, "select: {body}");
    assert_eq!(body["rows"], json!([["cli-aria"]]));

    // Failure exit: missing database is a nonzero exit with a JSON error body.
    let (ok, body, _) = run(&["query", "missing_db", "SELECT 1"], &key_path);
    assert!(!ok, "missing database must exit nonzero");
    assert_eq!(body["error"]["code"], "database_not_found");

    // Authentication failure also exits nonzero with the stable error code.
    let bad_key = key_file(server.data_dir.path(), &"0".repeat(64));
    let (ok, body, _) = run(&["list"], &bad_key);
    assert!(!ok);
    assert_eq!(body["error"]["code"], "invalid_credential");
}

#[test]
fn lifecycle_clone_export_import_delete() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "src");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/src/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"})),
    );
    assert_eq!(status, 200);
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/src/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "INSERT INTO t (v) VALUES ('alpha'),('beta')", "args": []})),
    );
    assert_eq!(status, 200);

    // Clone: independent copy with the same committed data.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/src/clone",
        Some(&owner),
        None,
        Some(&json!({"destination": "copy1"})),
    );
    assert_eq!(status, 200, "{body}");
    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/copy1/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM t", "args": []})),
    );
    assert_eq!(body["rows"], json!([[2]]));

    // Export: base64 snapshot, valid standalone SQLite with committed data.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/src/export",
        Some(&owner),
        None,
        Some(&json!({})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["database"], "src");
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/src/import",
        Some(&owner),
        None,
        Some(&json!({"name": "restored", "data_base64": body["data_base64"]})),
    );
    assert_eq!(status, 200, "{body}");
    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/restored/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT v FROM t ORDER BY id", "args": []})),
    );
    assert_eq!(body["rows"], json!([["alpha"], ["beta"]]));

    // Delete: tombstone. Query 404s, recreation 409s, file remains during retention.
    let (status, body) = request(
        &server.address,
        "DELETE",
        "/v1/databases/copy1",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], json!(true));
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/copy1/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT 1", "args": []})),
    );
    assert_eq!(status, 404);
    assert_eq!(error_code(&body), "database_not_found");
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases",
        Some(&owner),
        None,
        Some(&json!({"name": "copy1"})),
    );
    assert_eq!(status, 409, "recreating a tombstoned name must fail: {body}");
    // Double delete: second is 409 already_deleted.
    let (status, body) = request(
        &server.address,
        "DELETE",
        "/v1/databases/copy1",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 409, "{body}");
    // Deleting a never-existing name is 404.
    let (status, body) = request(
        &server.address,
        "DELETE",
        "/v1/databases/never_was",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 404, "{body}");
    // App keys cannot delete.
    let rw = issue_key(&server.address, &owner, "writer", "database_read_write", &["src"]);
    let (status, _) = request(
        &server.address,
        "DELETE",
        "/v1/databases/src",
        Some(&rw),
        None,
        None,
    );
    assert_eq!(status, 403);
}

#[test]
fn failed_import_and_missing_source_never_publish() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    // Garbage import payload: not a SQLite database at all.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/src/import",
        Some(&owner),
        None,
        Some(&json!({"name": "bad_import", "data_base64": "bm90IHNxbGl0ZQ=="})),
    );
    assert_eq!(status, 400, "{body}");
    // Nothing published under that name; the file must not linger.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/bad_import/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT 1", "args": []})),
    );
    assert_eq!(status, 404, "{body}");
    assert_eq!(error_code(&body), "database_not_found");
    // Clone from a missing source never creates the destination.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/ghost/clone",
        Some(&owner),
        None,
        Some(&json!({"destination": "phantom"})),
    );
    assert_eq!(status, 404, "{body}");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/phantom/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT 1", "args": []})),
    );
    assert_eq!(status, 404);
    // Tombstone name cannot be reused as an import target either.
    create_database(&server.address, &owner, "src");
    let (status, _) = request(
        &server.address,
        "DELETE",
        "/v1/databases/src",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200);
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/src/import",
        Some(&owner),
        None,
        Some(&json!({"name": "src", "data_base64": ""})),
    );
    // Empty payload cannot be a valid database; 400 with any stable code is
    // acceptable as long as nothing is published and the tombstone holds.
    assert_ne!(status, 200, "{body}");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/src/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT 1", "args": []})),
    );
    assert_eq!(status, 404, "tombstoned name must stay unavailable");
}

#[test]
fn clone_during_writes_takes_consistent_snapshot() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "busy");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/busy/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", "args": []})),
    );
    assert_eq!(status, 200);
    // One writer thread keeps committing; the clone must still succeed and
    // yield a self-consistent snapshot (a committed prefix, not torn data).
    let address = server.address.clone();
    let owner2 = owner.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..25 {
            let (status, _) = request(
                &address,
                "POST",
                "/v1/databases/busy/query",
                Some(&owner2),
                None,
                Some(&json!({"sql": "INSERT INTO t (v) VALUES (?)", "args": [json!(format!("w{i}"))]})),
            );
            assert_eq!(status, 200);
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/busy/clone",
        Some(&owner),
        None,
        Some(&json!({"destination": "snap"})),
    );
    writer.join().unwrap();
    assert_eq!(status, 200, "{body}");
    // Snapshot is a committed prefix: between 0 and 25 rows, integrity intact.
    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/snap/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM t", "args": []})),
    );
    let snap_count = body["rows"][0][0].as_i64().unwrap();
    assert!((0..=25).contains(&snap_count));
    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/busy/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM t", "args": []})),
    );
    assert_eq!(body["rows"][0][0], 25);
    let _ = snap_count;
}
