//! Non-interactive piped-stdin E2E for `dg shell` against a real server:
//! use → query → error → recovery → :batch from pasted JSON → .quit, plus
//! dot-command edge cases and explicit replay.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

fn key_file(dir: &std::path::Path, key: &str) -> std::path::PathBuf {
    let path = dir.join("shell.key");
    std::fs::write(&path, format!("{key}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

struct Server {
    child: std::process::Child,
    address: String,
    data_dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server() -> Server {
    let data_dir = tempfile::TempDir::new().unwrap();
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
    let mut lines = BufReader::new(stderr).lines();
    for line in lines.by_ref() {
        let line = line.unwrap();
        if let Ok(log) = serde_json::from_str::<Value>(&line)
            && let Some(addr) = log["fields"]["address"].as_str()
        {
            address = Some(addr.to_string());
            break;
        }
    }
    std::thread::spawn(move || for _ in lines {});
    Server {
        address: address.expect("server logged its listen address"),
        child,
        data_dir,
    }
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
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["owner_key"].as_str().unwrap().to_string()
}

/// Feed stdin lines to a fresh `dg shell`; returns stdout, stderr, exit success.
fn shell_session(server: &Server, key: &str, script: &[&str]) -> (String, String, bool) {
    shell_session_mode(server, key, script, false)
}

fn shell_session_mode(server: &Server, key: &str, script: &[&str], json_mode: bool) -> (String, String, bool) {
    let key_path = key_file(server.data_dir.path(), key);
    let mut command = Command::new(env!("CARGO_BIN_EXE_dg"));
    if json_mode { command.arg("--json"); }
    let mut child = command.args([
            "--server",
            &format!("http://{}", server.address),
            "--key-file",
            key_path.to_str().unwrap(),
            "shell",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut payload = script.join("\n");
    payload.push('\n');
    std::thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes());
    });
    let output = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

#[test]
fn shell_session_covers_use_query_error_recovery_batch_and_quit() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    let client = reqwest::blocking::Client::new();
    let status = client
        .post(format!("http://{}/v1/databases", server.address))
        .bearer_auth(&owner)
        .json(&json!({"name": "repldb"}))
        .send()
        .unwrap()
        .status();
    assert_eq!(status, 200);

    let script = vec![
        ".help",
        // Before any .use: statement is rejected, session continues.
        "SELECT 1",
        "use repldb",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO t (v) VALUES ('alpha'),('beta')",
        "SELECT id, v FROM t ORDER BY id",
        // Error path, then recovery in the same session.
        "SELECT * FROM missing_table",
        "SELECT count(*) FROM t",
        // :batch from pasted JSON.
        ":batch",
        r#"{"atomic":true,"statements":[{"sql":"INSERT INTO t (v) VALUES ('gamma')"},{"sql":"SELECT count(*) FROM t"}]}"#,
        // Keyed write via :key, then explicit replay produces the stored result.
        ":key repl-9",
        "INSERT INTO t (v) VALUES ('once')",
        ":key repl-9",
        "INSERT INTO t (v) VALUES ('once')",
        ":key",
        "SELECT count(*) FROM t WHERE v='once'",
        // Aligned table output assertions rely on these rows (see below).
        ".json",
        "SELECT id, v FROM t ORDER BY id",
        ".quit",
        "SELECT 'never executed'",
    ];
    let (stdout, stderr, ok) = shell_session(&server, &owner, &script);
    assert!(ok, "shell exits cleanly after .quit; stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    assert!(stdout.contains("dot-commands:"), ".help output: {stdout}");
    assert!(
        stdout.contains("no_database_selected"),
        "pre-use error surfaced: {stdout}"
    );
    // Aligned text result for the ordered SELECT.
    assert!(stdout.contains("id | v"), "table header: {stdout}");
    assert!(stdout.contains("1  | alpha"), "row rendering: {stdout}");
    assert!(stdout.contains("2  | beta"), "row rendering: {stdout}");
    assert!(
        stdout.contains("invalid_sql"),
        "failed statement printed with stable code: {stdout}"
    );
    let lines: Vec<_> = stdout.lines().map(str::trim_end).collect();
    assert!(lines.windows(3).any(|w| w == ["count(*)", "--------", "2"]), "recovery count: {stdout}");
    assert!(lines.windows(4).any(|w| w == ["statement 2:", "count(*)", "--------", "3"]), "batch result rendered: {stdout}");
    let result: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(result["rows"], json!([[1,"alpha"],[2,"beta"],[3,"gamma"],[4,"once"]]));
    assert!(
        !stdout.contains("never executed"),
        "no input after .quit is executed: {stdout}"
    );
}

#[test]
fn shell_errors_are_stable_and_never_kill_the_session() {
    let server = start_server();
    let owner = owner_bootstrap(&server);
    let client = reqwest::blocking::Client::new();
    for name in ["shella", "shellb"] {
        let status = client
            .post(format!("http://{}/v1/databases", server.address))
            .bearer_auth(&owner)
            .json(&json!({ "name": name }))
            .send()
            .unwrap()
            .status();
        assert_eq!(status, 200);
    }
    let script = vec![
        ".use shella",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        "INSERT INTO items (name) VALUES (NULL)", // constraint violation
        ".use shellb",
        "SELECT 'still here'",
        ".use ghost_db",                         // unknown database name accepted locally...
        "SELECT 1",                              // ...fails at query time, session survives
        ".use n0t-valid",                        // invalid name rejected locally
        ".bogus",                                // unknown dot command
        ".use shella",
        ".tables",
        ".schema items",
        "SELECT count(*) AS remaining FROM items",
        ".quit",
    ];
    let (stdout, stderr, ok) = shell_session(&server, &owner, &script);
    assert!(ok, "exit 0 after .quit; stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    assert!(stdout.contains("error [400] invalid_sql: SQL or parameters invalid"), "constraint error shown: {stdout}");
    assert!(stdout.contains("still here"), "session continues after .use switch: {stdout}");
    assert!(stdout.contains("database_not_found"), "ghost db error: {stdout}");
    assert!(stdout.contains("unknown_dot_command"), "unknown dot command: {stdout}");
    assert!(stdout.contains("CREATE TABLE items"), "schema inspection: {stdout}");
    assert!(!stdout.contains("_dg_replay"), "internal tables hidden: {stdout}");
    let lines: Vec<_> = stdout.lines().map(str::trim_end).collect();
    assert!(lines.windows(3).any(|w| w == ["remaining", "---------", "0"]), "constraint rolled back and session recovered: {stdout}");
}
