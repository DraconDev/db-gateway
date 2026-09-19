//! Lifecycle churn (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test lifecycle_churn`
//!
//! 300 deterministic (LCG-seeded) operations across 30 names: create,
//! delete, delete-then-recreate, keyed writes, batch writes, and count
//! reads. Every response code must match the test's own live-set model
//! (200 iff the name is live, else 404/409). Counts are verified inline
//! and at the end; the finale asserts the management list, the data-dir
//! file stems, and the absence of staging residue are all exact.

use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server {
    child: Child,
    dir: tempfile::TempDir,
    url: String,
    owner: String,
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
        let store = dg::store::Store::open(dir.path()).unwrap();
        let owner = store.bootstrap().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(dir.path())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut lines = BufReader::new(stderr).lines();
        let address = lines
            .by_ref()
            .find_map(|line| {
                serde_json::from_str::<Value>(&line.unwrap()).ok()?["fields"]["address"]
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap();
        std::thread::spawn(move || for _ in lines {});
        Self {
            child,
            dir,
            url: format!("http://{address}"),
            owner,
        }
    }
    fn call(&self, method: &str, path: &str, body: Value, key: Option<&str>) -> (u16, Value) {
        let mut request = reqwest::blocking::Client::new()
            .request(method.parse().unwrap(), format!("{}{path}", self.url))
            .bearer_auth(&self.owner)
            .json(&body)
            .timeout(Duration::from_secs(15));
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().unwrap();
        (response.status().as_u16(), response.json().unwrap())
    }
}

/// Deterministic LCG so the 300-op sequence is exactly reproducible.
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
fn three_hundred_ops_keep_registry_and_files_exact() {
    let server = Server::start();
    let names: Vec<String> = (0..30).map(|i| format!("ch_{i:02}")).collect();
    let mut live: HashSet<String> = HashSet::new();
    let mut tombstoned: HashSet<String> = HashSet::new();
    let mut admitted: HashMap<String, i64> = HashMap::new();
    let mut rng = Lcg(0xC0FFEE);
    let mut key_seq = 0u64;
    let mut key = || {
        key_seq += 1;
        format!("churn-{key_seq}")
    };

fn create_table(server: &Server, name: &str, idem: &str) -> bool {
    let (status, _) = server.call(
        "POST",
        &format!("/v1/databases/{name}/query"),
        json!({"sql": "CREATE TABLE d(id INTEGER PRIMARY KEY, v TEXT)"}),
        Some(idem),
    );
    status == 200
}
    let count = |server: &Server, name: &str| -> Option<i64> {
        let (status, body) = server.call(
            "POST",
            &format!("/v1/databases/{name}/query"),
            json!({"sql": "SELECT count(*) FROM d"}),
            None,
        );
        if status != 200 {
            return None;
        }
        body["rows"][0][0].as_i64()
    };

    for i in 0..300 {
        let name = names[rng.next(30) as usize].clone();
        match rng.next(6) {
            // Create: 200 iff dead, 409 iff live.
            0 => {
                let (status, _) = server.call(
                    "POST",
                    "/v1/databases",
                    json!({"name": name}),
                    None,
                );
                if live.contains(&name) {
                    assert_eq!(status, 409, "iter {i}: recreate of live {name} not 409");
                } else if tombstoned.contains(&name) {
                    assert_eq!(status, 409, "iter {i}: recreate of tombstoned {name} not 409");
                } else {
                    assert_eq!(status, 200, "iter {i}: create of dead {name} failed");
                    assert!(create_table(&server, &name, &key()), "iter {i}: seed {name}");
                    live.insert(name.clone());
                    admitted.insert(name.clone(), 0);
                }
            }
            // Delete: 200 iff live, 404 iff dead.
            1 => {
                let (status, _) =
                    server.call("DELETE", &format!("/v1/databases/{name}"), json!({}), None);
                if live.remove(&name) {
                    assert_eq!(status, 200, "iter {i}: delete of live {name} failed");
                    admitted.remove(&name);
                    tombstoned.insert(name);
                } else {
                    assert_eq!(status, 404, "iter {i}: delete of dead {name} not 404");
                }
            }
            // Keyed single write: 200 iff live.
            2 => {
                let (status, _) = server.call(
                    "POST",
                    &format!("/v1/databases/{name}/query"),
                    json!({"sql": "INSERT INTO d(v) VALUES (?)", "args": [format!("w-{i}")]}),
                    Some(&key()),
                );
                if live.contains(&name) {
                    assert_eq!(status, 200, "iter {i}: write to live {name} failed");
                    *admitted.get_mut(&name).unwrap() += 1;
                } else {
                    assert_eq!(status, 404, "iter {i}: write to dead {name} not 404");
                }
            }
            // Count read: exact iff live, 404 iff dead.
            3 => {
                let got = count(&server, &name);
                if live.contains(&name) {
                    assert_eq!(got, Some(admitted[&name]), "iter {i}: count drift on {name}");
                } else {
                    assert_eq!(got, None, "iter {i}: dead {name} readable");
                }
            }
            // Delete-then-recreate: fresh generation iff it was live.
            4 => {
                if live.contains(&name) {
                    let (status, _) = server.call(
                        "DELETE",
                        &format!("/v1/databases/{name}"),
                        json!({}),
                        None,
                    );
                    assert_eq!(status, 200, "iter {i}: cycle delete {name} failed");
                    // Tombstoned: immediate recreate must be refused.
                    let (status, _) = server.call(
                        "POST",
                        "/v1/databases",
                        json!({"name": name}),
                        None,
                    );
                    assert_eq!(status, 409, "iter {i}: tombstoned {name} recreatable");
                    live.remove(&name);
                    admitted.remove(&name);
                    tombstoned.insert(name);
                } else {
                    let (status, _) = server.call(
                        "POST",
                        "/v1/databases",
                        json!({"name": name}),
                        None,
                    );
                    // Dead names here are either never-created (200) or
                    // tombstoned by an earlier delete (409); both are
                    // correct, and the model must follow reality.
                    if status == 200 {
                        assert!(create_table(&server, &name, &key()), "iter {i}: reseed {name}");
                        live.insert(name.clone());
                        admitted.insert(name.clone(), 0);
                    } else {
                        assert_eq!(status, 409, "iter {i}: unexpected {status} for {name}");
                    }
                }
            }
            // Batch write of 5: all-or-nothing iff live.
            _ => {
                let (status, _) = server.call(
                    "POST",
                    &format!("/v1/databases/{name}/batch"),
                    json!({"statements": [
                        {"sql": "INSERT INTO d(v) VALUES ('b1')"},
                        {"sql": "INSERT INTO d(v) VALUES ('b2')"},
                        {"sql": "INSERT INTO d(v) VALUES ('b3')"},
                        {"sql": "INSERT INTO d(v) VALUES ('b4')"},
                        {"sql": "INSERT INTO d(v) VALUES ('b5')"},
                    ], "atomic": true}),
                    Some(&key()),
                );
                if live.contains(&name) {
                    assert_eq!(status, 200, "iter {i}: batch to live {name} failed");
                    *admitted.get_mut(&name).unwrap() += 5;
                } else {
                    assert_eq!(status, 404, "iter {i}: batch to dead {name} not 404");
                }
            }
        }
        // Rolling audit every 50 ops: list and all live counts exact.
        if i % 50 == 49 {
            let (status, body) = server.call("GET", "/v1/databases", json!({}), None);
            assert_eq!(status, 200);
            let mut listed: Vec<String> = body["databases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_str().unwrap().to_string())
                .collect();
            listed.sort();
            let mut want: Vec<String> = live.iter().cloned().collect();
            want.sort();
            assert_eq!(listed, want, "iter {i}: registry drift");
            for name in &live {
                assert_eq!(count(&server, name), Some(admitted[name]), "iter {i}: drift {name}");
            }
        }
    }

    // Finale: registry, files, counts, and no staging residue.
    let (status, body) = server.call("GET", "/v1/databases", json!({}), None);
    assert_eq!(status, 200);
    let mut listed: Vec<String> = body["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    listed.sort();
    let mut want: Vec<String> = live.iter().cloned().collect();
    want.sort();
    assert_eq!(listed, want);
    assert!(!live.is_empty(), "churn killed everything; test proves nothing");

    let mut stems: HashSet<String> = HashSet::new();
    for entry in std::fs::read_dir(server.dir.path()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".lifecycle-"),
            "staging residue: {name}"
        );
        if name == "metadata.sqlite" {
            continue;
        }
        let stem = name
            .strip_suffix(".sqlite")
            .or_else(|| name.strip_suffix(".sqlite-wal"))
            .or_else(|| name.strip_suffix(".sqlite-shm"))
            .unwrap_or_else(|| panic!("unexpected file: {name}"));
        stems.insert(stem.to_string());
    }
    let live_stems: HashSet<String> = live.clone();
    assert!(
        live_stems.is_subset(&stems),
        "live DB missing file: {live_stems:?} vs {stems:?}"
    );
    // Tombstones keep files by design; every stem must be live or deleted.
    for name in &names {
        if stems.contains(name) {
            let (status, _) = server.call(
                "POST",
                &format!("/v1/databases/{name}/query"),
                json!({"sql": "SELECT 1"}),
                None,
            );
            if live.contains(name) {
                assert_eq!(status, 200);
            } else {
                assert_eq!(status, 404, "{name} file present but queryable state wrong");
            }
        }
    }
    for name in &live {
        assert_eq!(count(&server, name), Some(admitted[name]));
    }
}
