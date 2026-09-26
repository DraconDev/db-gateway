//! `dg web` launch contract: the console auto-starts its gateway.
//!
//! Run: `timeout 200 cargo test --locked --test console_launch`
//!
//! Failure modes are deterministic CLI asserts. The positive launch drives
//! a stub companion plus a real auto-started gateway through `dg web`:
//! env passthrough, readiness URL print, HTTP serving, and SIGTERM
//! supervision with neither child orphaned. A held port proves an
//! already-running gateway is used as-is (no spawn). The real companion
//! is covered by web/test/browser.test.mjs and scripts/dg-web-smoke.mjs.

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

/// SIGTERM `dg web` and wait for a clean exit.
fn sigterm_and_wait(mut child: std::process::Child) {
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
}

fn port_closed(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_secs(2),
    )
    .is_err()
}

#[test]
fn console_launch_autostarts_gateway_and_supervises_both() {
    if !node_available() {
        eprintln!(
            "skipping console_launch_autostarts_gateway_and_supervises_both: node not on PATH"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let stub = write_stub(dir.path());
    let port = free_port();
    let gateway_port = free_port();
    let gateway_url = format!("http://127.0.0.1:{gateway_port}");
    // Deliberately nonexistent: `dg web` forwards the path without
    // reading key material, and the stub never reads it either.
    let key_path = dir.path().join("unread.key");
    let data_dir = dir.path().join("auto-data");
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
            &gateway_url,
            "--data-dir",
            data_dir.to_str().unwrap(),
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

    // A real gateway was auto-started into the data dir: it answers
    // unauthenticated liveness on the target port.
    assert!(data_dir.is_dir(), "auto-started server owns the data dir");
    let live = reqwest::blocking::get(format!("{gateway_url}/health/live")).unwrap();
    assert_eq!(live.status(), 200);

    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("env.json")).unwrap())
            .unwrap();
    assert_eq!(record["DG_SERVER"], gateway_url);
    assert_eq!(record["DG_KEY_FILE"], key_path.to_str().unwrap());
    assert_eq!(record["DG_CONSOLE_PORT"], port.to_string());
    assert_eq!(record["DG_CONSOLE_HOST"], "127.0.0.1");

    // SIGTERM supervision: dg exits cleanly and both children are gone.
    sigterm_and_wait(child);
    assert!(
        port_closed(port),
        "companion port must be closed after supervised shutdown"
    );
    assert!(
        port_closed(gateway_port),
        "auto-started gateway port must be closed after supervised shutdown"
    );
}

#[test]
fn web_with_reachable_gateway_starts_no_server() {
    if !node_available() {
        eprintln!("skipping web_with_reachable_gateway_starts_no_server: node not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let stub = write_stub(dir.path());
    let port = free_port();
    // A held port reads as a running gateway: anything listening means
    // `dg web` must not spawn (a spawn here would fail to bind and the
    // command would error instead of printing ready).
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway_port = held.local_addr().unwrap().port();
    let key_path = dir.path().join("unread.key");
    let data_dir = dir.path().join("must-stay-absent");
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
            &format!("http://127.0.0.1:{gateway_port}"),
            "--data-dir",
            data_dir.to_str().unwrap(),
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
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("dg web printed its ready line");
    assert!(
        !data_dir.exists(),
        "no server spawn means no data dir creation"
    );
    sigterm_and_wait(child);
    drop(held);
}

#[test]
fn unstartable_gateway_fails_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let stub = write_stub(dir.path());
    let output = dg()
        .args([
            "web",
            "--port",
            "18787",
            "--key-file",
            "dummy.key",
            "--entry",
            stub.to_str().unwrap(),
            "--server",
            "http://127.0.0.1:9",
            "--data-dir",
            dir.path().join("data").to_str().unwrap(),
        ])
        .env_remove("DG_CONSOLE_HOST")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gateway_startup_failed"),
        "stderr: {stderr}"
    );
}
