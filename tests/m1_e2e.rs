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
    let mut lines = BufReader::new(stderr).lines();
    let mut address = None;
    for line in lines.by_ref() {
        let line = line.unwrap();
        if let Ok(log) = serde_json::from_str::<Value>(&line)
            && let Some(addr) = log["fields"]["address"].as_str()
        {
            address = Some(addr.to_string());
            break;
        }
    }
    // The address is logged at bind time, but bind is not ready: the
    // router is still being built behind it, and on a loaded box the
    // first request used to meet a connection refused. Poll the
    // unauthenticated liveness endpoint instead of assuming the port
    // answers. The leftover log lines are drained in the background
    // so a chatty server can never block on a full stderr pipe.
    let address = address.expect("server logged its listen address");
    std::thread::spawn(move || for _ in lines.by_ref() {});
    let probe = reqwest::blocking::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let ready = probe
            .get(format!("http://{address}/health/live"))
            .timeout(Duration::from_secs(2))
            .send()
            .is_ok_and(|response| response.status().is_success());
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server at {address} never became ready"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Server {
        address,
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
        .timeout(Duration::from_secs(60));
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

/// Deadline failure over HTTP: stable 408/code, server recovers for normal
/// writes, and the interrupted statement leaves no partial rows.
#[test]
fn deadline_failure_recovers_over_http() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "game_a");

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE heavy (v INTEGER)"})),
    );
    assert_eq!(status, 200, "ddl: {body}");

    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "INSERT INTO heavy VALUES (7)"})),
    );
    assert_eq!(status, 200, "seed: {body}");

    // Recursive CTE far past the 2s execution deadline.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(
            &json!({"sql": "INSERT INTO heavy WITH RECURSIVE c(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM c) SELECT v FROM c"}),
        ),
    );
    assert_eq!(status, 408, "deadline status: {body}");
    assert_eq!(error_code(&body), "query_deadline_exceeded");

    // A normal request succeeds immediately after the failure.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "INSERT INTO heavy VALUES (8)"})),
    );
    assert_eq!(status, 200, "post-failure write: {body}");
    assert_eq!(body["rows_affected"], 1);

    // Exact data: seed + recovery row only, no partial CTE rows.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/game_a/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT v FROM heavy ORDER BY v"})),
    );
    assert_eq!(status, 200, "post-failure read: {body}");
    assert_eq!(body["rows"], json!([[7], [8]]));
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

    // Management routes are denied to app keys: create, issue, revoke.
    // Listing is scoped instead of denied (asserted below).
    for (method, path, body) in [
        ("POST", "/v1/databases", Some(json!({"name": "sneaky"}))),
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

    // Scoped listing: each app key sees exactly its authorized databases.
    for key in [&rw, &ro] {
        let (status, body) = request(
            &server.address,
            "GET",
            "/v1/databases",
            Some(key),
            None,
            None,
        );
        assert_eq!(status, 200, "scoped list: {body}");
        assert_eq!(body["databases"], json!(["one"]));
    }
    let (status, body) = request(
        &server.address,
        "GET",
        "/v1/databases",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200, "owner list: {body}");
    assert_eq!(body["databases"], json!(["one", "two"]));

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
fn request_result_boundaries_reject_cleanly_and_recover() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "bounds");

    let query = |sql: &str| {
        request(
            &server.address,
            "POST",
            "/v1/databases/bounds/query",
            Some(&owner),
            None,
            Some(&json!({"sql": sql})),
        )
    };
    let batch = |statements: Vec<Value>| {
        request(
            &server.address,
            "POST",
            "/v1/databases/bounds/batch",
            Some(&owner),
            None,
            Some(&json!({"atomic": true, "statements": statements})),
        )
    };
    let select = || json!({"sql": "SELECT 1"});

    let (status, body) = query("CREATE TABLE t (v INTEGER)");
    assert_eq!(status, 200, "ddl: {body}");
    let (status, body) = query("INSERT INTO t VALUES (7)");
    assert_eq!(status, 200, "seed: {body}");

    // Batch statement count: 32 admitted, 33 rejected, empty rejected.
    let (status, body) = batch((0..32).map(|_| select()).collect());
    assert_eq!(status, 200, "32-statement batch: {body}");
    let (status, body) = batch((0..33).map(|_| select()).collect());
    assert_eq!(status, 400, "33-statement batch: {body}");
    assert_eq!(error_code(&body), "invalid_request");
    let (status, body) = batch(vec![]);
    assert_eq!(status, 400, "empty batch: {body}");
    assert_eq!(error_code(&body), "invalid_request");
    // A rejected oversized batch with a leading write commits nothing.
    let mut writes: Vec<Value> = vec![json!({"sql": "INSERT INTO t VALUES (8)"})];
    writes.extend((0..32).map(|_| select()));
    let (status, body) = batch(writes);
    assert_eq!(status, 400, "write-carrying oversized batch: {body}");
    assert_eq!(error_code(&body), "invalid_request");
    let (status, body) = query("SELECT v FROM t");
    assert_eq!(status, 200, "post-rejection read: {body}");
    assert_eq!(body["rows"], json!([[7]]));

    // SQL text length over HTTP: a 60 KiB statement fits the 64 KiB JSON body
    // cap and succeeds; a 70 KiB statement is rejected at the body cap with
    // 413. The exact 65536/65537 SQL-text boundary is covered at unit level.
    let (status, body) = query(&format!("SELECT '{}'", "x".repeat(60_000)));
    assert_eq!(status, 200, "60 KiB SQL: {body}");
    let (status, body) = query(&format!("SELECT '{}'", "x".repeat(70_000)));
    assert_eq!(status, 413, "over-cap SQL body: {body}");
    assert_eq!(error_code(&body), "invalid_request");
    let (status, body) = query("");
    assert_eq!(status, 400, "empty SQL: {body}");
    assert_eq!(error_code(&body), "invalid_request");

    // Argument count: 256 admitted, 257 rejected (single result column so
    // the 128-column output cap does not mask the argument boundary).
    let placeholders = |n: usize| vec!["?"; n].join("+");
    let args = |n: usize| (1..=n as i64).collect::<Vec<_>>();
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/bounds/query",
        Some(&owner),
        None,
        Some(&json!({"sql": format!("SELECT {}", placeholders(256)), "args": args(256)})),
    );
    assert_eq!(status, 200, "256 args: {body}");
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/bounds/query",
        Some(&owner),
        None,
        Some(&json!({"sql": format!("SELECT {}", placeholders(257)), "args": args(257)})),
    );
    assert_eq!(status, 400, "257 args: {body}");
    assert_eq!(error_code(&body), "invalid_request");

    // Row count: exactly 1000 rows admitted, 1001 rejected with 413.
    let (status, body) = query(
        "INSERT INTO t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM s WHERE v < 999) SELECT v FROM s",
    );
    assert_eq!(status, 200, "seed to 1000 rows: {body}");
    let (status, body) = query("SELECT v FROM t");
    assert_eq!(status, 200, "1000-row result: {body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 1000);
    let (status, _) = query("INSERT INTO t VALUES (1001)");
    assert_eq!(status, 200);
    let (status, body) = query("SELECT v FROM t");
    assert_eq!(status, 413, "1001-row result: {body}");
    assert_eq!(error_code(&body), "result_limit_exceeded");

    // Byte size: one 300 KiB value in a single row exceeds 256 KiB with 413.
    // The value is generated server-side so the small request stays under
    // the 64 KiB JSON body cap.
    let (status, body) = query("CREATE TABLE big (v TEXT)");
    assert_eq!(status, 200, "ddl: {body}");
    let (status, body) = query("INSERT INTO big SELECT hex(randomblob(150000))");
    assert_eq!(status, 200, "seed big value: {body}");
    let (status, body) = query("SELECT v FROM big");
    assert_eq!(status, 413, "oversized-value result: {body}");
    assert_eq!(error_code(&body), "result_limit_exceeded");

    // Recovery: a normal write and exact read succeed after every rejection.
    let (status, body) = query("CREATE TABLE ok (v INTEGER)");
    assert_eq!(status, 200, "post-rejection ddl: {body}");
    let (status, body) = query("INSERT INTO ok VALUES (1)");
    assert_eq!(status, 200, "post-rejection write: {body}");
    assert_eq!(body["rows_affected"], 1);
    let (status, body) = query("SELECT v FROM ok");
    assert_eq!(status, 200, "post-rejection read: {body}");
    assert_eq!(body["rows"], json!([[1]]));
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

    let ro = issue_key(
        &server.address,
        &owner,
        "copy_reader",
        "database_read_only",
        &["copy1", "src"],
    );
    let rw_copy = issue_key(
        &server.address,
        &owner,
        "copy_writer",
        "database_read_write",
        &["copy1", "src"],
    );
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
    let (status, listed) = request(
        &server.address,
        "GET",
        "/v1/databases",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(listed["databases"], json!(["restored", "src"]));
    assert!(server.data_dir.path().join("copy1.sqlite").exists());
    for key in [&ro, &rw_copy] {
        let (status, body) = request(
            &server.address,
            "POST",
            "/v1/databases/copy1/query",
            Some(key),
            None,
            Some(&json!({"sql":"SELECT 1"})),
        );
        assert_eq!(status, 404, "{body}");
        assert_eq!(error_code(&body), "database_not_found");
        // The credential itself survives and retains access to its other database.
        assert_eq!(
            request(
                &server.address,
                "POST",
                "/v1/databases/src/query",
                Some(key),
                None,
                Some(&json!({"sql":"SELECT 1"}))
            )
            .0,
            200
        );
    }
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
    assert_eq!(
        status, 409,
        "recreating a tombstoned name must fail: {body}"
    );
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
    let rw = issue_key(
        &server.address,
        &owner,
        "writer",
        "database_read_write",
        &["src"],
    );
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
    // Hold a real uncommitted multi-statement WAL transaction throughout
    // the HTTP clone. The clone must see the exact previous commit, never
    // either half of the pending transaction; no sleep establishes overlap.
    let mut writer =
        rusqlite::Connection::open(server.data_dir.path().join("busy.sqlite")).unwrap();
    writer
        .execute_batch("INSERT INTO t VALUES (1,'committed'),(2,'pair');")
        .unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    transaction
        .execute("UPDATE t SET v='pending' WHERE id=1", [])
        .unwrap();
    transaction
        .execute("INSERT INTO t VALUES (3,'pending_pair')", [])
        .unwrap();
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/busy/clone",
        Some(&owner),
        None,
        Some(&json!({"destination": "snap"})),
    );
    assert!(
        !transaction.is_autocommit(),
        "writer transaction overlaps the completed clone"
    );
    assert_eq!(status, 200, "{body}");
    transaction.commit().unwrap();
    // Exact committed snapshot, including values, not just a row-count range.
    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/snap/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT id,v FROM t ORDER BY id", "args": []})),
    );
    assert_eq!(body["rows"], json!([[1, "committed"], [2, "pair"]]));
    let (_, body) = request(
        &server.address,
        "POST",
        "/v1/databases/busy/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT count(*) FROM t", "args": []})),
    );
    assert_eq!(body["rows"][0][0], 3);
    let snapshot = rusqlite::Connection::open(server.data_dir.path().join("snap.sqlite")).unwrap();
    assert_eq!(
        snapshot
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn cli_lifecycle_commands_against_running_server() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    let key_path = key_file(server.data_dir.path(), &owner);
    let run = |args: &[&str], key: &std::path::Path| {
        dgcli(&format!("http://{}", server.address), key, args)
    };

    let (ok, body, _) = run(&["create", "clidb2"], &key_path);
    assert!(ok, "{body}");
    let (ok, body, _) = run(
        &[
            "query",
            "clidb2",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        ],
        &key_path,
    );
    assert!(ok, "{body}");
    let (ok, body, _) = run(
        &[
            "query",
            "clidb2",
            "INSERT INTO t (v) VALUES ('one'),('two')",
        ],
        &key_path,
    );
    assert!(ok, "{body}");

    // dg clone
    let (ok, body, _) = run(&["clone", "clidb2", "clcopy"], &key_path);
    assert!(ok, "clone: {body}");
    assert_eq!(body["destination"], "clcopy");

    // dg export -> file -> verify content independently
    let out = server.data_dir.path().join("snap.sqlite");
    let (ok, body, _) = run(
        &["export", "clidb2", "--out", out.to_str().unwrap()],
        &key_path,
    );
    assert!(ok, "export: {body}");
    assert_eq!(body["name"], "clidb2");
    assert!(out.is_file());
    let snapshot = rusqlite::Connection::open(&out).unwrap();
    assert_eq!(
        snapshot
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        snapshot
            .query_row("SELECT group_concat(v, ',') FROM t", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "one,two"
    );
    drop(snapshot);

    // dg import
    let (ok, body, _) = run(
        &["import", "reimported", "--from", out.to_str().unwrap()],
        &key_path,
    );
    assert!(ok, "import: {body}");
    let (ok, _body, stdout) = run(
        &["query", "reimported", "SELECT count(*) FROM t"],
        &key_path,
    );
    // The import path argument IS the new database name: 'reimported'.
    assert!(ok, "query reimported: {stdout}");
    assert_eq!(_body["rows"], json!([[2]]));

    // Exact typed-name confirmation is required; absent/wrong values send no DELETE.
    for args in [
        vec!["delete", "clcopy"],
        vec!["delete", "clcopy", "--confirm", "wrong"],
        vec!["delete", "clcopy", "--yes"],
    ] {
        let (ok, _, stdout) = run(&args, &key_path);
        assert!(!ok, "invalid confirmation must fail");
        assert!(stdout.trim().is_empty(), "stdout: {stdout}");
        assert!(run(&["query", "clcopy", "SELECT 1"], &key_path).0);
    }
    let (ok, body, _) = run(&["delete", "clcopy", "--confirm", "clcopy"], &key_path);
    assert!(ok, "delete --confirm: {body}");
    assert_eq!(body["deleted"], json!(true));
    let (ok, body, _) = run(&["query", "clcopy", "SELECT 1"], &key_path);
    assert!(!ok);
    assert_eq!(body["error"]["code"], "database_not_found");
}

#[test]
fn delete_with_active_writer_is_safe_and_never_recreates() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    create_database(&server.address, &owner, "victim");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases/victim/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "CREATE TABLE t (id INTEGER PRIMARY KEY)", "args": []})),
    );
    assert_eq!(status, 200);
    // Hold an actual uncommitted writer through the HTTP delete response.
    // The retained file must remain usable by this already-open connection.
    let mut writer =
        rusqlite::Connection::open(server.data_dir.path().join("victim.sqlite")).unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    transaction.execute("INSERT INTO t VALUES (1)", []).unwrap();
    let (status, body) = request(
        &server.address,
        "DELETE",
        "/v1/databases/victim",
        Some(&owner),
        None,
        None,
    );
    assert!(
        !transaction.is_autocommit(),
        "writer is active when deletion returns"
    );
    assert_eq!(status, 200, "{body}");
    transaction.commit().unwrap();
    assert_eq!(
        writer
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        writer
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let (status, listed) = request(
        &server.address,
        "GET",
        "/v1/databases",
        Some(&owner),
        None,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(listed["databases"], json!([]));
    // Deleted name: 404, and recreation is refused while the tombstone holds.
    let (status, body) = request(
        &server.address,
        "POST",
        "/v1/databases/victim/query",
        Some(&owner),
        None,
        Some(&json!({"sql": "SELECT 1", "args": []})),
    );
    assert_eq!(status, 404);
    assert_eq!(error_code(&body), "database_not_found");
    let (status, _) = request(
        &server.address,
        "POST",
        "/v1/databases",
        Some(&owner),
        None,
        Some(&json!({"name": "victim"})),
    );
    assert_eq!(status, 409);
}
