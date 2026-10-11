//! Match-result finalization: a retry cannot double-reward.
//!
//! Run: `timeout 300 cargo test --locked --test match_finalization`
//!
//! Scope: this test pins the contract an application finalizing a match
//! result can rely on. Three rules are asserted exactly:
//!
//! 1. A retry of the same finalization under the same `Idempotency-Key`
//!    replays the original result byte-for-byte; the row count and the
//!    `sum(reward_amount)` are not incremented a second time.
//! 2. The same key with a different body is `409
//!    idempotency_key_conflict` and changes nothing — an attacker (or a
//!    bug) cannot redirect a finalized reward to a different player by
//!    reusing the key.
//! 3. The finalization lives in a single database; the gateway does
//!    not promise cross-database atomicity, and the test does not
//!    attempt to write across databases. Multi-database "finalization"
//!    patterns must implement their own saga.
//!
//! The single-writer-per-database model (`docs/architecture.md` M1
//! design, `docs/api.md` "Write semantics, Idempotency-Key, retries")
//! is what makes rule 1 hold: a concurrent second writer for the same
//! row cannot race the keyed replay. The model is the precondition
//! for the test, not a property the test proves; an integration
//! boundary drill (`tests/m2_lifecycle.rs` contended_capacity_rejects)
//! covers the writer-fairness side.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
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

fn spawn(data_dir: &std::path::Path) -> (Server, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
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
    (
        Server {
            child,
            address: address.expect("server logged its listen address"),
        },
        owner,
    )
}

fn query(address: &str, cred: &str, db: &str, body: &Value, idem: Option<&str>) -> (u16, Value) {
    let mut request = reqwest::blocking::Client::new()
        .post(format!("http://{address}/v1/databases/{db}/query"))
        .bearer_auth(cred)
        .json(body)
        .timeout(Duration::from_secs(30));
    if let Some(idem) = idem {
        request = request.header("Idempotency-Key", idem);
    }
    let response = request.send().unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

fn batch(address: &str, cred: &str, db: &str, body: &Value, idem: Option<&str>) -> (u16, Value) {
    let mut request = reqwest::blocking::Client::new()
        .post(format!("http://{address}/v1/databases/{db}/batch"))
        .bearer_auth(cred)
        .json(body)
        .timeout(Duration::from_secs(30));
    if let Some(idem) = idem {
        request = request.header("Idempotency-Key", idem);
    }
    let response = request.send().unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

#[test]
fn retry_of_a_match_finalization_under_the_same_key_does_not_double_reward() {
    let data_dir = tempfile::tempdir().unwrap();
    let (server, owner) = spawn(data_dir.path());
    let a = server.address.clone();

    // Single database holds both the match record and its reward — the
    // gateway's single-writer-per-database model is what makes a
    // cross-table finalization atomic. Cross-database patterns are out
    // of scope: see the module docs.
    let (status, body) = reqwest::blocking::Client::new()
        .post(format!("http://{a}/v1/databases"))
        .bearer_auth(&owner)
        .json(&json!({"name": "league_a"}))
        .timeout(Duration::from_secs(30))
        .send()
        .map(|r| (r.status().as_u16(), r.json::<Value>().unwrap()))
        .unwrap();
    assert_eq!(status, 200, "create league_a: {body}");

    for ddl in [
        "CREATE TABLE matches (id INTEGER PRIMARY KEY, winner TEXT NOT NULL, played_at TEXT NOT NULL)",
        "CREATE TABLE rewards (match_id INTEGER PRIMARY KEY, player TEXT NOT NULL, amount INTEGER NOT NULL)",
    ] {
        let (status, body) = query(
            &a,
            &owner,
            "league_a",
            &json!({"sql": ddl}),
            Some(&format!("ddl-{}", ddl.len())),
        );
        assert_eq!(status, 200, "ddl: {body}");
    }

    // App-scoped read-write credential, scoped to league_a only.
    let (status, body) = reqwest::blocking::Client::new()
        .post(format!("http://{a}/v1/keys"))
        .bearer_auth(&owner)
        .json(&json!({"name": "finalizer", "role": "database_read_write", "databases": ["league_a"]}))
        .timeout(Duration::from_secs(30))
        .send()
        .map(|r| (r.status().as_u16(), r.json::<Value>().unwrap()))
        .unwrap();
    assert_eq!(status, 200, "issue finalizer: {body}");
    let finalizer = body["key"].as_str().unwrap().to_string();

    // The match is decided once: the finalization is a single
    // transaction with two statements (the match record + the reward
    // row), keyed by the match id. Any retry under the same key must
    // replay the original result, not re-execute the transaction.
    let match_id = 9182_i64;
    let finalization = json!({
        "atomic": true,
        "statements": [
            {"sql": "INSERT INTO matches (id, winner, played_at) VALUES (?, ?, ?)", "args": [match_id, "aria", "2026-10-11T00:00:00Z"]},
            {"sql": "INSERT INTO rewards (match_id, player, amount) VALUES (?, ?, ?)", "args": [match_id, "aria", 100]}
        ]
    });

    // First call: 200, both rows present, sum(rewards) = 100.
    let (status, first) = batch(
        &a,
        &finalizer,
        "league_a",
        &finalization,
        Some(&format!("match-final-{match_id}")),
    );
    assert_eq!(status, 200, "first finalization: {first}");
    let (status, body) = query(
        &a,
        &finalizer,
        "league_a",
        &json!({"sql": "SELECT count(*) FROM matches WHERE id = ?", "args": [match_id]}),
        None,
    );
    assert_eq!(status, 200, "match count: {body}");
    assert_eq!(body["rows"][0][0].as_i64().unwrap(), 1, "match row missing");
    let (status, body) = query(
        &a,
        &finalizer,
        "league_a",
        &json!({"sql": "SELECT count(*), coalesce(sum(amount), 0) FROM rewards WHERE match_id = ?", "args": [match_id]}),
        None,
    );
    assert_eq!(status, 200, "reward sum: {body}");
    assert_eq!(
        body["rows"][0][0].as_i64().unwrap(),
        1,
        "exactly one reward row"
    );
    assert_eq!(
        body["rows"][0][1].as_i64().unwrap(),
        100,
        "reward amount recorded"
    );

    // Retry 1: same key, same body, transport died last time — the
    // server must replay the original result, NOT re-execute the
    // transaction. count(matches) stays 1, sum(rewards) stays 100.
    let (status, replay1) = batch(
        &a,
        &finalizer,
        "league_a",
        &finalization,
        Some(&format!("match-final-{match_id}")),
    );
    assert_eq!(status, 200, "replay 1: {replay1}");
    assert_eq!(replay1, first, "replay 1 bytes drifted from first");

    // Retry 2: still the same key + body, simulating a second client
    // that re-sent the request after a load-balancer retry.
    let (status, replay2) = batch(
        &a,
        &finalizer,
        "league_a",
        &finalization,
        Some(&format!("match-final-{match_id}")),
    );
    assert_eq!(status, 200, "replay 2: {replay2}");
    assert_eq!(replay2, first, "replay 2 bytes drifted from first");

    // Final state: still exactly one match row and exactly one reward
    // row of 100. No double-reward.
    let (status, body) = query(
        &a,
        &finalizer,
        "league_a",
        &json!({"sql": "SELECT count(*) FROM matches WHERE id = ?", "args": [match_id]}),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["rows"][0][0].as_i64().unwrap(),
        1,
        "match row duplicated by retry"
    );
    let (status, body) = query(
        &a,
        &finalizer,
        "league_a",
        &json!({"sql": "SELECT count(*), coalesce(sum(amount), 0) FROM rewards WHERE match_id = ?", "args": [match_id]}),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["rows"][0][0].as_i64().unwrap(),
        1,
        "reward row duplicated by retry"
    );
    assert_eq!(
        body["rows"][0][1].as_i64().unwrap(),
        100,
        "reward amount doubled by retry"
    );

    // Same key, different body: 409, the finalization is NOT silently
    // redirected to a different player or amount. The original row
    // stays; the rejected request changes nothing.
    let hijack = json!({
        "atomic": true,
        "statements": [
            {"sql": "INSERT INTO matches (id, winner, played_at) VALUES (?, ?, ?)", "args": [match_id, "kestrel", "2026-10-11T00:00:00Z"]},
            {"sql": "INSERT INTO rewards (match_id, player, amount) VALUES (?, ?, ?)", "args": [match_id, "kestrel", 9999]}
        ]
    });
    let (status, body) = batch(
        &a,
        &finalizer,
        "league_a",
        &hijack,
        Some(&format!("match-final-{match_id}")),
    );
    assert_eq!(status, 409, "key-conflict: {body}");
    assert_eq!(body["error"]["code"], "idempotency_key_conflict");
    let (status, body) = query(
        &a,
        &finalizer,
        "league_a",
        &json!({"sql": "SELECT winner FROM matches WHERE id = ?", "args": [match_id]}),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["rows"][0][0].as_str().unwrap(),
        "aria",
        "winner was overwritten by a key-replay attempt"
    );
}
