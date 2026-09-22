//! Scratch probe (not committed): capture burst-failure bodies.
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
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

#[test]
fn probe_burst_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    let owner = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            dir.path().to_str().unwrap(),
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
    let server = Server {
        child,
        address: address.unwrap(),
    };
    reqwest::blocking::Client::new()
        .post(format!("http://{}/v1/databases", server.address))
        .bearer_auth(&owner)
        .json(&json!({"name": "burst"}))
        .send()
        .unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let barrier = barrier.clone();
            let address = server.address.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::new();
                barrier.wait();
                let start = std::time::Instant::now();
                let response = client
                    .post(format!("http://{address}/v1/databases/burst/query"))
                    .bearer_auth(&owner)
                    .json(&json!({
                        "sql": "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x<300000) SELECT count(*) FROM cnt",
                        "args": [],
                    }))
                    .timeout(Duration::from_secs(30))
                    .send()
                    .unwrap();
                let status = response.status().as_u16();
                let body = response.text().unwrap_or_default();
                format!("t{i} {status} after {:?}: {body}", start.elapsed())
            })
        })
        .collect();
    for h in handles {
        eprintln!("{}", h.join().unwrap());
    }
}
