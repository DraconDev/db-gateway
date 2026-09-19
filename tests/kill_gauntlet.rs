//! Kill gauntlet: 20 SIGKILLs during mixed churn (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test kill_gauntlet`
//!
//! Twenty rounds share one data dir. Each round creates a fresh database,
//! runs LCG-driven mixed ops (keyed single writes, keyed 5-row batches,
//! unkeyed reads), then SIGKILLs the server while one keyed write is in
//! flight. After restart every unknown-outcome key is replayed explicitly
//! (same key, identical body — the documented outcome_unknown recovery,
//! never a blind retry) and the row count must equal admitted singles
//! plus admitted batches plus replayed unknowns: zero unaccounted rows.
//! Every restart must also serve traffic immediately and list the full
//! registry. A write acked 200 before the kill is committed
//! (synchronous=FULL, ack after commit) and can never be lost.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Server {
    child: Child,
    address: String,
}

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

fn send(
    address: &str,
    key: &str,
    path: &str,
    idem: &str,
    body: &Value,
) -> Result<(u16, Value), String> {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(key)
        .header("Idempotency-Key", idem)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    Ok((status, body))
}

fn read(address: &str, key: &str, db: &str, sql: &str) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}/v1/databases/{db}/query"))
        .bearer_auth(key)
        .json(&json!({"sql": sql}))
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

/// Deterministic LCG so the 20-round sequence is exactly reproducible.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, modulus: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % modulus
    }
}

#[test]
fn twenty_kills_leave_zero_unaccounted_rows() {
    let data_dir = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();

    let mut rng = Lcg(0x6A0071E7);
    // Per-DB admitted rows, across restarts (replay store lives in the file).
    let mut admitted: Vec<i64> = vec![0; 20];

    for round in 0..20 {
        let db = format!("g{round:02}");
        let mut server = spawn_on(data_dir.path());
        let (status, body) = send(
            &server.address,
            &owner,
            "/v1/databases",
            &format!("create-{db}"),
            &json!({"name": db}),
        )
        .unwrap();
        assert_eq!(status, 200, "round {round} create: {body}");
        let (status, _) = read(&server.address, &owner, &db, "CREATE TABLE k(id INTEGER PRIMARY KEY, tag TEXT)");
        assert_eq!(status, 200);

        let ops = 15 + rng.next(25);
        let kill_slot = rng.next(ops);
        let kill_delay_ms = rng.next(16);
        let mut unknowns: Vec<(String, Value)> = Vec::new();

        for i in 0..ops {
            let tag = format!("r{round}-{i}");
            let idem = format!("g-{round}-{i}");
            if i == kill_slot {
                // In-flight write: fire on a thread, SIGKILL mid-flight.
                let (address, key, path, body) = (
                    server.address.clone(),
                    owner.clone(),
                    format!("/v1/databases/{db}/query"),
                    json!({"sql": "INSERT INTO k(tag) VALUES (?)", "args": [tag]}),
                );
                let handle = std::thread::spawn(move || send(&address, &key, &path, &idem, &body));
                std::thread::sleep(Duration::from_millis(kill_delay_ms));
                server.child.kill().unwrap();
                server.child.wait().unwrap();
                let idem_replay = format!("g-{round}-{i}");
                let body_replay = json!({"sql": "INSERT INTO k(tag) VALUES (?)", "args": [tag]});
                match handle.join().unwrap() {
                    Ok((200, _)) => admitted[round] += 1,
                    Ok((status, body)) => panic!("round {round}: kill-slot wrote {status}: {body}"),
                    Err(_) => unknowns.push((idem_replay, body_replay)),
                }
                break;
            }
            match rng.next(3) {
                0 => {
                    let (status, body) = send(
                        &server.address,
                        &owner,
                        &format!("/v1/databases/{db}/query"),
                        &idem,
                        &json!({"sql": "INSERT INTO k(tag) VALUES (?)", "args": [tag]}),
                    )
                    .unwrap();
                    assert_eq!(status, 200, "round {round} op {i}: {body}");
                    admitted[round] += 1;
                }
                1 => {
                    let (status, body) = send(
                        &server.address,
                        &owner,
                        &format!("/v1/databases/{db}/batch"),
                        &idem,
                        &json!({"statements": [
                            {"sql": "INSERT INTO k(tag) VALUES ('a')"},
                            {"sql": "INSERT INTO k(tag) VALUES ('b')"},
                            {"sql": "INSERT INTO k(tag) VALUES ('c')"},
                            {"sql": "INSERT INTO k(tag) VALUES ('d')"},
                            {"sql": "INSERT INTO k(tag) VALUES ('e')"},
                        ], "atomic": true}),
                    )
                    .unwrap();
                    assert_eq!(status, 200, "round {round} op {i}: {body}");
                    admitted[round] += 5;
                }
                _ => {
                    let (status, _) = read(&server.address, &owner, &db, "SELECT count(*) FROM k");
                    assert_eq!(status, 200, "round {round} op {i}: read failed");
                }
            }
        }

        // Restart on the same dir: registry intact, traffic served at once.
        server = spawn_on(data_dir.path());
        let (status, body) = read(&server.address, &owner, &db, "SELECT count(*) FROM k");
        assert_eq!(status, 200, "round {round}: dead after restart: {body}");
        // Explicit replay of every unknown outcome, then exact accounting.
        for (idem, body) in &unknowns {
            let (status, _) = send(
                &server.address,
                &owner,
                &format!("/v1/databases/{db}/query"),
                idem,
                body,
            )
            .unwrap();
            assert_eq!(status, 200, "round {round}: replay {idem} failed");
            admitted[round] += 1;
        }
        let (status, body) = read(&server.address, &owner, &db, "SELECT count(*) FROM k");
        assert_eq!(status, 200, "round {round}: final count failed: {body}");
        assert_eq!(
            body["rows"],
            json!([[admitted[round]]]),
            "round {round}: unaccounted rows"
        );
        server.child.kill().unwrap();
        server.child.wait().unwrap();
    }

    // Finale: all 20 databases present with exact counts, no more, no less.
    let server = spawn_on(data_dir.path());
    let response = reqwest::blocking::Client::new()
        .get(format!("http://{}/v1/databases", server.address))
        .bearer_auth(&owner)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let list: Value = response.json().unwrap();
    let mut names: Vec<String> = list["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    names.sort();
    let want: Vec<String> = (0..20).map(|r| format!("g{r:02}")).collect();
    assert_eq!(names, want);
    for (round, total) in admitted.iter().enumerate() {
        let (status, body) = read(&server.address, &owner, &format!("g{round:02}"), "SELECT count(*) FROM k");
        assert_eq!(status, 200);
        assert_eq!(body["rows"], json!([[*total]]), "g{round:02} drifted");
    }
}
