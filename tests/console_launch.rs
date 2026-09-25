//! `dg web` launch contract (disposable stubs, no gateway needed).
//!
//! Run: `timeout 200 cargo test --locked --test console_launch`
//!
//! Failure modes are deterministic CLI asserts. The positive launch drives
//! a stub companion through the real `dg web` supervisor: env
//! passthrough, readiness URL print, HTTP serving, and SIGTERM supervision
//! with no orphaned child. The real companion is covered by
//! web/test/browser.test.mjs and scripts/web-console-smoke.mjs.

use std::{
    io::{BufRead, BufReader},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

fn dg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dg"))
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Stub companion: records its env, serves 200 on /, exits on SIGTERM.
fn write_stub(dir: &std::path::Path) -> PathBuf {
    let entry = dir.join("stub-companion.mjs");
    let record = dir.join("env.json");
    let source = format!(
        "import {{ createServer }} from 'node:http';\n\
         import {{ writeFileSync }} from 'node:fs';\n\
         writeFileSync({record:?}, JSON.stringify({{ \
         DG_SERVER: process.env.DG_SERVER ?? null, \
         DG_KEY_FILE: process.env.DG_KEY_FILE ?? null, \
         DG_CONSOLE_PORT: process.env.DG_CONSOLE_PORT ?? null, \
         DG_CONSOLE_HOST: process.env.DG_CONSOLE_HOST ?? null }}));\n\
         createServer((req, res) => {{ \
         res.writeHead(200, {{ 'content-type': 'text/plain' }}); \
         res.end('stub console'); \
         }}).listen(Number(process.env.DG_CONSOLE_PORT), '127.0.0.1');\n"
    );
    std::fs::write(&entry, source).unwrap();
    entry
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn invalid_console_port_rejected() {
    let output = dg().args(["web", "--port", "80"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid_console_port"), "stderr: {stderr}");
}

#[test]
fn missing_credential_without_key_file() {
    let home = tempfile::tempdir().unwrap();
    let output = dg()
        .env("HOME", home.path())
        .env_remove("DG_KEY_FILE")
        .args(["web", "--port", "18787"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing_credential"), "stderr: {stderr}");
}

#[test]
fn missing_companion_entry_reports_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let missing_key = dir.path().join("missing.key");
    let output = dg()
        .args([
            "web",
            "--port",
            "18787",
            "--key-file",
            missing_key.to_str().unwrap(),
            "--entry",
            "/nonexistent-console/server.js",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid_console_entry")
            && stderr.contains("/nonexistent-console/server.js"),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_node_runtime_reports_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let stub = write_stub(dir.path());
    let empty_path = tempfile::tempdir().unwrap();
    let output = dg()
        .env("PATH", empty_path.path())
        .args([
            "web",
            "--port",
            "18787",
            "--key-file",
            "dummy.key",
            "--entry",
            stub.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("console_runtime_unavailable"),
        "stderr: {stderr}"
    );
}

#[test]
fn console_launch_serves_and_supervises_child() {
    if !node_available() {
        eprintln!("skipping console_launch_serves_and_supervises_child: node not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let stub = write_stub(dir.path());
    let port = free_port();
    // Deliberately nonexistent: `dg web` forwards the path without
    // reading key material, and the stub never reads it either.
    let key_path = dir.path().join("unread.key");
    let mut child = dg()
        .args([
            "web",
            "--port",
            &port.to_string(),
            "--key-file",
            key_path.to_str().unwrap(),
            "--entry",
            stub.to_str().unwrap(),
            "--server",
            "http://127.0.0.1:9",
        ])
        .env_remove("DG_CONSOLE_HOST")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("dg web ready:") {
                let _ = ready_tx.send(line);
                return;
            }
        }
    });
    let ready = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("dg web printed its ready line");
    assert!(
        ready.contains(&format!("http://127.0.0.1:{port}")),
        "ready line: {ready}"
    );

    let body = reqwest::blocking::get(format!("http://127.0.0.1:{port}/"))
        .unwrap()
        .text()
        .unwrap();
    assert_eq!(body, "stub console");

    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("env.json")).unwrap())
            .unwrap();
    assert_eq!(record["DG_SERVER"], "http://127.0.0.1:9");
    assert_eq!(record["DG_KEY_FILE"], key_path.to_str().unwrap());
    assert_eq!(record["DG_CONSOLE_PORT"], port.to_string());
    assert_eq!(record["DG_CONSOLE_HOST"], "127.0.0.1");

    // SIGTERM supervision: dg exits cleanly and the child is gone (no orphan).
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let (exit_tx, exit_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = exit_tx.send(child.wait());
    });
    let status = exit_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("dg web exits after SIGTERM")
        .unwrap();
    assert!(status.success(), "status: {status}");
    assert!(
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_secs(2)
        )
        .is_err(),
        "companion port must be closed after supervised shutdown"
    );
}
