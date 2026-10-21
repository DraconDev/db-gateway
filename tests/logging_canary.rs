//! Logging canary: credentials and SQL values must never reach logs.
//!
//! Run: `timeout 120 cargo test --locked --test logging_canary`
//!
//! Captures real server stderr plus CLI stderr across success and failure
//! paths, then asserts no canary appears. Intentional outputs (query results
//! on stdout, issued keys on stdout) are asserted present so the test fails
//! if someone "fixes" leakage by suppressing them.

use std::{
    io::Read,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};

const ROW_CANARY: &str = "canary-row-value-3b77e1";
const ARG_CANARY: &str = "canary-sql-arg-c41d88";
const SQL_CANARY: &str = "canary_sql_text_9d4e";
// 64 hex chars so it passes the credential shape check and reaches auth.
const KEY_CANARY: &str = "c0ffee11c0ffee11c0ffee11c0ffee11c0ffee11c0ffee11c0ffee11c0ffee11";

struct Server {
    child: Child,
    dir: tempfile::TempDir,
    address: String,
    logs: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(dir.path())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = child.stderr.take().unwrap();
        // The listen address is the readiness signal; buffer everything after
        // it too so the audit sees complete server output.
        let mut first = Vec::new();
        let address = loop {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stderr.read_exact(&mut byte).unwrap();
                first.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Ok(log) = serde_json::from_str::<Value>(&line)
                && let Some(addr) = log["fields"]["address"].as_str()
            {
                break addr.to_string();
            }
        };
        let logs = Arc::new(Mutex::new(first));
        let writer = logs.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => writer.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            dir,
            address,
            logs,
            reader: Some(reader),
        }
    }

    /// Kill the server and return every stderr byte it ever wrote. Joining the
    /// reader after process exit (pipe EOF) means no log line is in flight.
    fn stop(mut self) -> String {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        String::from_utf8_lossy(&self.logs.lock().unwrap()).into_owned()
    }

    fn call(&self, method: &str, path: &str, key: &str, body: Value) -> (u16, Value) {
        let response = reqwest::blocking::Client::new()
            .request(
                method.parse().unwrap(),
                format!("http://{}{path}", self.address),
            )
            .bearer_auth(key)
            .json(&body)
            .timeout(Duration::from_secs(60))
            .send()
            .unwrap();
        (response.status().as_u16(), response.json().unwrap())
    }
}

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

#[test]
fn server_and_cli_logs_exclude_credential_and_sql_canaries() {
    let server = Server::start();
    let address = server.address.clone();

    // Bootstrap through the CLI: the owner key is intentional stdout, and
    // becomes credential canary #1 for every later step.
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", server.dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let init_logs = String::from_utf8(output.stderr).unwrap();
    assert!(
        !init_logs.contains(&owner),
        "init stderr leaks the owner credential"
    );

    let db = "/v1/databases/vault/query";
    let (status, _) = server.call("POST", "/v1/databases", &owner, json!({"name": "vault"}));
    assert_eq!(status, 200);
    let (status, _) = server.call(
        "POST",
        db,
        &owner,
        json!({"sql": "CREATE TABLE secrets (v TEXT)"}),
    );
    assert_eq!(status, 200);
    // Success path carrying every SQL-value canary shape: literal, argument,
    // and a failing statement that echoes the canary back in no log.
    let (status, _) = server.call(
        "POST",
        db,
        &owner,
        json!({"sql": "INSERT INTO secrets VALUES (?)", "args": [ARG_CANARY]}),
    );
    assert_eq!(status, 200);
    let (status, body) = server.call(
        "POST",
        db,
        &owner,
        json!({"sql": format!("INSERT INTO secrets VALUES ('{ROW_CANARY}')")}),
    );
    assert_eq!(status, 200, "{body}");
    let (status, _) = server.call(
        "POST",
        db,
        &owner,
        json!({"sql": format!("SELECT {SQL_CANARY} FROM secrets")}),
    );
    assert_eq!(status, 400);
    let (status, _) = server.call("POST", db, KEY_CANARY, json!({"sql": "SELECT 1"}));
    assert_eq!(status, 401);
    let (status, _) = server.call(
        "POST",
        "/v1/databases/missing/query",
        &owner,
        json!({"sql": "SELECT 1"}),
    );
    assert_eq!(status, 404);

    // CLI failure path: canary SQL through the real CLI transport.
    let cli_key = key_file(server.dir.path(), &owner);
    let cli_fail = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["--server", &format!("http://{address}"), "--key-file"])
        .arg(&cli_key)
        .args([
            "query",
            "vault",
            &format!("SELECT {SQL_CANARY} FROM secrets"),
        ])
        .output()
        .unwrap();
    assert!(!cli_fail.status.success());
    let cli_stderr = String::from_utf8(cli_fail.stderr).unwrap();
    assert!(
        !cli_stderr.contains(SQL_CANARY),
        "CLI stderr echoes SQL text: {cli_stderr}"
    );
    assert!(
        !cli_stderr.contains(&owner),
        "CLI stderr echoes the credential"
    );

    // Intentional outputs stay intact: canary row on stdout, issued key on
    // stdout, nothing sensitive on stderr.
    let cli_ok = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["--server", &format!("http://{address}"), "--key-file"])
        .arg(&cli_key)
        .args(["query", "vault", "SELECT v FROM secrets"])
        .output()
        .unwrap();
    assert!(cli_ok.status.success());
    let cli_stdout = String::from_utf8(cli_ok.stdout).unwrap();
    assert!(
        cli_stdout.contains(ROW_CANARY) && cli_stdout.contains(ARG_CANARY),
        "query results must remain on stdout: {cli_stdout}"
    );
    assert!(
        String::from_utf8(cli_ok.stderr).unwrap().is_empty(),
        "successful CLI must not log to stderr"
    );
    let cli_issue = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["--server", &format!("http://{address}"), "--key-file"])
        .arg(&cli_key)
        .args([
            "key",
            "create",
            "reader",
            "--role",
            "database_read_only",
            "--database",
            "vault",
        ])
        .output()
        .unwrap();
    assert!(cli_issue.status.success());
    let issued: String = serde_json::from_slice::<Value>(&cli_issue.stdout).unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(issued.len(), 64);

    // The audit: no canary anywhere in captured server or CLI stderr.
    let server_logs = server.stop();
    assert!(
        server_logs.contains("listening"),
        "capture is vacuous without the listen line: {server_logs}"
    );
    for canary in [
        owner.as_str(),
        issued.as_str(),
        ROW_CANARY,
        ARG_CANARY,
        SQL_CANARY,
        KEY_CANARY,
    ] {
        assert!(
            !server_logs.contains(canary),
            "server stderr leaks canary {canary:?}: {server_logs}"
        );
        assert!(
            !cli_stderr.contains(canary),
            "CLI stderr leaks canary {canary:?}"
        );
        assert!(
            !init_logs.contains(canary),
            "init stderr leaks canary {canary:?}"
        );
    }
}
