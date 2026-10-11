//! Disk-pressure drill: file growth is capped, so the writer dies
//! mid-transaction and the data directory must recover on restart.
//!
//! Run: `timeout 120 cargo test --locked --test disk_pressure`
//!
//! A real `dg serve` gets its file-size ceiling slammed down with
//! `prlimit` (the kernel refuses to grow the database files, the way a
//! full disk refuses growth) while it is inserting. The write that hits
//! the ceiling kills the writer with SIGXFSZ; acknowledged rows must
//! survive restart exactly and the fresh server must serve again once the
//! ceiling is gone with the dead process.
//!
//! What this models honestly: abrupt writer death under growth pressure
//! plus WAL recovery on restart. It does NOT model the polite errno path
//! (SQLite reporting SQLITE_FULL to a live process); that mapping is
//! locked by the `sqlite_full_maps_to_stable_internal_error` unit test in
//! src/error.rs. An earlier revision of this file tried to impose the
//! pressure with `PRAGMA max_page_count`, which does not work black-box:
//! the pragma is connection-local in this SQLite build (a fresh
//! connection reads back the default), so the server's own connections
//! would never see the ceiling.
//!
//! Synchronization is HTTP acknowledgement plus the restarted server's
//! address log line — no sleeps. SIGXFSZ is 25 on Linux; the test asserts
//! the death came from that signal, not from a random crash.

use std::{
    io::{BufRead, BufReader},
    os::unix::process::ExitStatusExt,
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;

const SIGXFSZ: i32 = 25;
/// File-size ceiling in bytes: above the fresh files (~16-37 KiB) so the
/// baseline lands first, low enough that a few 4 KiB inserts trip it.
const CEILING_BYTES: &str = "49152";

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

fn request(address: &str, method: &str, path: &str, key: &str, body: &Value) -> (u16, Value) {
    try_request(address, method, path, key, body).expect("transport error on an expected-ok call")
}

fn try_request(
    address: &str,
    method: &str,
    path: &str,
    key: &str,
    body: &Value,
) -> Result<(u16, Value), String> {
    let response = reqwest::blocking::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(key)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    Ok((status, body))
}

#[test]
fn file_size_ceiling_kills_the_writer_but_not_the_acknowledged_rows() {
    let data_dir = TempDir::new().unwrap();
    let mut server = spawn_on(data_dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    for db in ["pressure", "control"] {
        let (status, body) = request(
            &server.address,
            "POST",
            "/v1/databases",
            &owner,
            &json!({"name": db}),
        );
        assert_eq!(status, 200, "create {db}: {body}");
        let (status, body) = request(
            &server.address,
            "POST",
            &format!("/v1/databases/{db}/query"),
            &owner,
            &json!({"sql": "CREATE TABLE payload (id INTEGER PRIMARY KEY, blob TEXT NOT NULL)"}),
        );
        assert_eq!(status, 200, "ddl {db}: {body}");
    }
    // Acknowledged BEFORE the ceiling: the durability barrier.
    for i in 0..3 {
        let (status, body) = request(
            &server.address,
            "POST",
            "/v1/databases/pressure/query",
            &owner,
            &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": [format!("base-{i}")]}),
        );
        assert_eq!(status, 200, "baseline {i}: {body}");
    }

    // Impose the ceiling on the live server: no file in the process may
    // grow past CEILING_BYTES. (Pipes and sockets are unaffected, so the
    // test's HTTP reads still work; only growth dies.)
    let prlimit = Command::new("prlimit")
        .args([
            format!("--fsize={CEILING_BYTES}"),
            "--pid".to_string(),
            server.child.id().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        prlimit.status.success(),
        "prlimit failed: {}",
        String::from_utf8_lossy(&prlimit.stderr)
    );

    // Fill until the writer dies. Which write trips the ceiling is up to
    // the kernel; the test only requires that growth DOES kill it.
    let chunk = "y".repeat(4_000);
    let mut killed_at = None;
    for i in 0..50 {
        match try_request(
            &server.address,
            "POST",
            "/v1/databases/pressure/query",
            &owner,
            &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": [chunk]}),
        ) {
            Ok((status, body)) => assert_eq!(status, 200, "fill {i}: {body}"),
            Err(_) => {
                killed_at = Some(i);
                break;
            }
        }
    }
    assert!(
        killed_at.is_some(),
        "file-size ceiling never killed the writer in 50 inserts"
    );

    // The death was the ceiling (SIGXFSZ), not a random crash.
    let status = server.child.wait().unwrap();
    assert_eq!(
        status.signal(),
        Some(SIGXFSZ),
        "server died by signal {status:?}, want SIGXFSZ"
    );

    // Restart on the same directory: the ceiling died with the process.
    let restarted = spawn_on(data_dir.path());
    let (http, body) = request(
        &restarted.address,
        "POST",
        "/v1/databases/pressure/query",
        &owner,
        &json!({"sql": "SELECT id, blob FROM payload ORDER BY id"}),
    );
    assert_eq!(http, 200, "post-restart read: {body}");
    let rows = body["rows"].as_array().unwrap();
    let baseline: Vec<Value> =
        json!([[1, "base-0"], [2, "base-1"], [3, "base-2"]]).as_array().unwrap().clone();
    assert!(
        rows.len() >= 3,
        "acknowledged rows lost: {rows:?}"
    );
    assert_eq!(&rows[..3], &baseline[..], "baseline exact, no partial write");
    // Every surviving row past the baseline was itself acknowledged: ids
    // are dense from 1 with no gaps a rolled-back write could leave behind
    // as anything but a trailing absence.
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(row[0], json!((n + 1) as i64), "dense ids, no gaps: {rows:?}");
    }

    // The fresh server serves writes again on both databases.
    for db in ["pressure", "control"] {
        let (http, body) = request(
            &restarted.address,
            "POST",
            &format!("/v1/databases/{db}/query"),
            &owner,
            &json!({"sql": "INSERT INTO payload (blob) VALUES (?)", "args": ["after-recovery"]}),
        );
        assert_eq!(http, 200, "write after restart on {db}: {body}");
    }
}
