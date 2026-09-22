//! Admission: bursty arrivals wait instead of failing (scale proposal Phase 1).
//!
//! Run: `timeout 300 cargo test --locked --test admission`
//!
//! Five barrier-synced threads hit one database with ~50 ms queries at the
//! same instant. With zero-wait `try_acquire` only one would succeed and the
//! rest would see 429; with bounded-wait admission all five must succeed
//! because the worst-case queue wait (~4 x single latency) fits inside
//! ACQUIRE_TIMEOUT (500 ms).

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
    time::Duration,
};

use serde_json::{Value, json};

/// Threads fired at the same instant; one more than the global permit count
/// so both the global wait path and the per-database wait path are covered.
const THREADS: usize = 5;
/// Recursive-CTE size calibrated so one request takes ~50 ms: five
/// serialized holders then wait at most ~250 ms, inside the 500 ms timeout.
const CTE_ROWS: usize = 300_000;

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

#[test]
fn burst_waits_instead_of_shedding() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{}/v1/databases", server.address))
        .bearer_auth(&owner)
        .json(&json!({"name": "burst"}))
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = barrier.clone();
            let address = server.address.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::new();
                barrier.wait();
                let response = client
                    .post(format!("http://{address}/v1/databases/burst/query"))
                    .bearer_auth(&owner)
                    .json(&json!({
                        "sql": format!("WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x<{CTE_ROWS}) SELECT count(*) FROM cnt"),
                        "args": [],
                    }))
                    .timeout(Duration::from_secs(30))
                    .send()
                    .unwrap();
                response.status().as_u16()
            })
        })
        .collect();
    let mut statuses: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        vec![200; THREADS],
        "every thread in the burst must be admitted within the bounded wait"
    );
}

/// Scale proposal Phase 2: eight concurrent reads share the read lane and
/// the shared per-database guard instead of contending for one slot.
#[test]
fn eight_concurrent_reads_run_retry_free() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{}/v1/databases", server.address))
        .bearer_auth(&owner)
        .json(&json!({"name": "reads"}))
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    const READERS: usize = 8;
    let barrier = Arc::new(Barrier::new(READERS));
    let handles: Vec<_> = (0..READERS)
        .map(|_| {
            let barrier = barrier.clone();
            let address = server.address.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::new();
                barrier.wait();
                let response = client
                    .post(format!("http://{address}/v1/databases/reads/query"))
                    .bearer_auth(&owner)
                    .json(&json!({"sql": "SELECT 1", "args": []}))
                    .timeout(Duration::from_secs(30))
                    .send()
                    .unwrap();
                response.status().as_u16()
            })
        })
        .collect();
    let mut statuses: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        vec![200; READERS],
        "concurrent reads must share the read lane with zero 429s"
    );
}

/// Scale proposal Phase 2: reads arriving during a slow write wait on the
/// shared guard (bounded by ACQUIRE_TIMEOUT) instead of failing instantly.
#[test]
fn reads_proceed_during_slow_write() {
    let dir = tempfile::tempdir().unwrap();
    let owner = init(dir.path());
    let server = start(dir.path());
    let client = reqwest::blocking::Client::new();
    let post = |path: &str, body: &Value| {
        client
            .post(format!("http://{}{path}", server.address))
            .bearer_auth(&owner)
            .json(body)
            .timeout(Duration::from_secs(30))
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(post("/v1/databases", &json!({"name": "mixed"})), 200);
    assert_eq!(
        post(
            "/v1/databases/mixed/query",
            &json!({"sql": "CREATE TABLE t (x)", "args": []})
        ),
        200
    );

    // One slow write (~30k inserted rows) holds the exclusive guard while
    // four reads arrive mid-write; every request must succeed.
    let address = server.address.clone();
    let owner_write = owner.clone();
    let writer = std::thread::spawn(move || {
        reqwest::blocking::Client::new()
            .post(format!("http://{address}/v1/databases/mixed/query"))
            .bearer_auth(&owner_write)
            .json(&json!({
                "sql": "INSERT INTO t WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x<30000) SELECT x FROM cnt",
                "args": [],
            }))
            .timeout(Duration::from_secs(30))
            .send()
            .unwrap()
            .status()
            .as_u16()
    });
    std::thread::sleep(Duration::from_millis(50));
    const READERS: usize = 4;
    let barrier = Arc::new(Barrier::new(READERS));
    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let barrier = barrier.clone();
            let address = server.address.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::new();
                barrier.wait();
                client
                    .post(format!("http://{address}/v1/databases/mixed/query"))
                    .bearer_auth(&owner)
                    .json(&json!({"sql": "SELECT count(*) FROM t", "args": []}))
                    .timeout(Duration::from_secs(30))
                    .send()
                    .unwrap()
                    .status()
                    .as_u16()
            })
        })
        .collect();
    let mut statuses = vec![writer.join().unwrap()];
    statuses.extend(readers.into_iter().map(|h| h.join().unwrap()));
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        vec![200; READERS + 1],
        "slow write plus overlapping reads must all succeed"
    );
}
