//! Clone marathon (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test clone_marathon`
//!
//! A real `dg serve` holds one dummy source database. Ten sequential
//! clones must each match the source dump byte-for-byte. Then a
//! background writer hammers the source with keyed single-row INSERTs
//! while ten more clones fire: each loaded clone must be a contiguous
//! id-prefix snapshot within [initial, final] bounds, and the source
//! must end with exactly initial + admitted rows, original rows
//! untouched. A final write to one clone proves copy independence.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;

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
    // Keep draining stderr for the life of the server: dropping the reader
    // here closes the pipe, and the next line the server logs then fails on a
    // closed stderr. That window is invisible on an idle box and shows up as a
    // spurious connection refusal under full-suite load.
    std::thread::spawn(move || {
        for line in lines {
            if line.is_err() {
                break;
            }
        }
    });
    Server {
        child,
        address: address.expect("server logged its listen address"),
    }
}

fn post(address: &str, method: &str, path: &str, key: &str, body: &Value) -> (u16, Value) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    post_keyed(address, method, path, key, &format!("auto-{n}"), body)
}

fn post_keyed(
    address: &str,
    method: &str,
    path: &str,
    key: &str,
    idem: &str,
    body: &Value,
) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(key)
        .header("Idempotency-Key", idem)
        .json(body)
        .timeout(Duration::from_secs(60))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

fn query(address: &str, key: &str, db: &str, sql: &str) -> (u16, Value) {
    post(
        address,
        "POST",
        &format!("/v1/databases/{db}/query"),
        key,
        &json!({"sql": sql}),
    )
}

fn dump(address: &str, key: &str, db: &str) -> Value {
    let (status, body) = query(
        address,
        key,
        db,
        "SELECT id, name, score, hex(payload), note FROM d ORDER BY id",
    );
    assert_eq!(status, 200, "dump {db}: {body}");
    body["rows"].clone()
}

#[test]
fn twenty_clones_with_write_load_stay_exact() {
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

    let server = spawn_on(data_dir.path());
    let (status, body) = post(
        &server.address,
        "POST",
        "/v1/databases",
        &owner,
        &json!({"name": "orig"}),
    );
    assert_eq!(status, 200, "create: {body}");

    // 200 mixed-type dummy rows in one atomic batch of 32-statement chunks.
    let mut seed: Vec<Value> = vec![
        json!({"sql": "CREATE TABLE d(id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, payload BLOB, note TEXT)"}),
    ];
    for i in 0..200 {
        seed.push(json!({"sql": format!("INSERT INTO d(name, score, payload, note) VALUES ('seed-{i}', {}, randomblob(16), {})", i as f64 * 0.25, if i % 3 == 0 { "NULL" } else { "'n'" })}));
    }
    for (c, chunk) in seed.chunks(32).enumerate() {
        let (status, body) = post(
            &server.address,
            "POST",
            "/v1/databases/orig/batch",
            &owner,
            &json!({"statements": chunk, "atomic": true}),
        );
        assert_eq!(status, 200, "seed chunk {c}: {body}");
    }
    let base = dump(&server.address, &owner, "orig");
    assert_eq!(base.as_array().unwrap().len(), 200);

    // Phase A: ten sequential clones, each byte-comparable to the source.
    for i in 0..10 {
        let name = format!("c_seq_{i}");
        let (status, body) = post(
            &server.address,
            "POST",
            "/v1/databases/orig/clone",
            &owner,
            &json!({"destination": name}),
        );
        assert_eq!(status, 200, "clone {name}: {body}");
        assert_eq!(
            dump(&server.address, &owner, &name),
            base,
            "clone {name} differs"
        );
    }

    // Phase B: background keyed writer plus ten clones under load.
    let stop = Arc::new(AtomicBool::new(false));
    let admitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (w_addr, w_key) = (server.address.clone(), owner.clone());
    let (w_stop, w_adm) = (stop.clone(), admitted.clone());
    let writer = std::thread::spawn(move || {
        let mut n = 0;
        while !w_stop.load(Ordering::Relaxed) {
            let tag = format!("load-{n}");
            let (status, _) = post_keyed(
                &w_addr,
                "POST",
                "/v1/databases/orig/query",
                &w_key,
                &format!("load-key-{n}"),
                &json!({"sql": "INSERT INTO d(name, score) VALUES (?, ?)", "args": [tag.clone(), n]}),
            );
            if status == 200 {
                w_adm.lock().unwrap().push(tag);
            }
            n += 1;
        }
    });
    let mut loaded_counts = Vec::new();
    for i in 0..10 {
        let name = format!("c_load_{i}");
        let (status, body) = post(
            &server.address,
            "POST",
            "/v1/databases/orig/clone",
            &owner,
            &json!({"destination": name}),
        );
        assert_eq!(status, 200, "clone {name}: {body}");
        let rows = dump(&server.address, &owner, &name);
        loaded_counts.push(rows.as_array().unwrap().len());
        // Snapshot consistency: contiguous id prefix starting at 1.
        let ids: Vec<i64> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect();
        assert_eq!(
            ids,
            (1..=ids.len() as i64).collect::<Vec<_>>(),
            "clone {name} not a prefix snapshot"
        );
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let admitted = admitted.lock().unwrap();
    assert!(
        !admitted.is_empty(),
        "background writer admitted nothing; load phase untested"
    );

    // Source untouched by cloning: original 200 rows identical, total exact.
    let final_dump = dump(&server.address, &owner, "orig");
    let final_rows = final_dump.as_array().unwrap();
    assert_eq!(
        final_rows.len(),
        200 + admitted.len(),
        "source lost or gained rows"
    );
    assert_eq!(
        Value::Array(final_rows[..200].to_vec()),
        base,
        "original rows mutated"
    );
    // Every loaded clone is a prefix snapshot within [200, final] bounds.
    for (i, n) in loaded_counts.iter().enumerate() {
        assert!(
            (200..=final_rows.len()).contains(n),
            "clone c_load_{i} count {n} out of bounds"
        );
        let rows = dump(&server.address, &owner, &format!("c_load_{i}"));
        assert_eq!(
            rows,
            Value::Array(final_rows[..*n].to_vec()),
            "clone c_load_{i} not a source prefix"
        );
    }

    // Copy independence: write to a clone, source count unchanged.
    let (status, body) = post(
        &server.address,
        "POST",
        "/v1/databases/c_load_0/query",
        &owner,
        &json!({"sql": "INSERT INTO d(name) VALUES ('clone-only')"}),
    );
    assert_eq!(status, 200, "clone write: {body}");
    let (status, body) = query(&server.address, &owner, "orig", "SELECT count(*) FROM d");
    assert_eq!(status, 200, "source count: {body}");
    assert_eq!(body["rows"], json!([[200 + admitted.len()]]));
}
