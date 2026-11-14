//! Replay deep dive (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test replay_deep`
//!
//! Five behaviors, all asserted exactly:
//! 1. Post-restart replay persistence: a keyed write replayed after a
//!    clean restart returns byte-identical results without re-executing.
//! 2. Key cap: the key after the 10 000th per principal per database is
//!    429 `capacity_exceeded`, while existing keys still replay 200.
//! 3. Same key + different body is 409 `idempotency_key_conflict` and
//!    changes nothing.
//! 4. Cross-database and cross-principal isolation: identical key
//!    strings are independent namespaces; no cross-talk.
//! 5. Replay bytes identical: a keyed read replayed after the data
//!    changed returns the original bytes (replay, never re-execution).

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
        .timeout(Duration::from_secs(60));
    if let Some(idem) = idem {
        request = request.header("Idempotency-Key", idem);
    }
    let response = request.send().unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

fn count(address: &str, cred: &str, db: &str) -> i64 {
    let (status, body) = query(
        address,
        cred,
        db,
        &json!({"sql": "SELECT count(*) FROM t"}),
        None,
    );
    assert_eq!(status, 200, "{db} count: {body}");
    body["rows"][0][0].as_i64().unwrap()
}

#[test]
fn replay_contract_holds_across_restart_cap_conflict_and_namespaces() {
    let data_dir = tempfile::tempdir().unwrap();
    let (mut server, owner) = spawn(data_dir.path());
    let a = server.address.clone();

    let create = |db: &str| {
        let (status, body) = reqwest::blocking::Client::new()
            .post(format!("http://{a}/v1/databases"))
            .bearer_auth(&owner)
            .json(&json!({"name": db}))
            .timeout(Duration::from_secs(60))
            .send()
            .map(|r| (r.status().as_u16(), r.json::<Value>().unwrap()))
            .unwrap();
        assert_eq!(status, 200, "create {db}: {body}");
    };
    create("ra");
    create("rb");
    for db in ["ra", "rb"] {
        let (status, body) = query(
            &a,
            &owner,
            db,
            &json!({"sql": "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"}),
            Some(&format!("ddl-{db}")),
        );
        assert_eq!(status, 200, "{db} ddl: {body}");
    }
    // Second write-capable principal scoped to both databases.
    let (status, body) = reqwest::blocking::Client::new()
        .post(format!("http://{a}/v1/keys"))
        .bearer_auth(&owner)
        .json(&json!({"name": "app", "role": "database_read_write", "databases": ["ra", "rb"]}))
        .timeout(Duration::from_secs(60))
        .send()
        .map(|r| (r.status().as_u16(), r.json::<Value>().unwrap()))
        .unwrap();
    assert_eq!(status, 200, "issue app: {body}");
    let app = body["key"].as_str().unwrap().to_string();

    // 1+5. Keyed write, then keyed read whose replay must go stale by design.
    let write_body = json!({"sql": "INSERT INTO t(v) VALUES ('deep')", "args": []});
    let (status, write_v1) = query(&a, &owner, "ra", &write_body, Some("deep-write"));
    assert_eq!(status, 200, "{write_v1}");
    let read_body = json!({"sql": "SELECT count(*) FROM t"});
    let (status, read_v1) = query(&a, &owner, "ra", &read_body, Some("deep-read"));
    assert_eq!(status, 200, "{read_v1}");

    // Clean restart: replay store must survive (it lives in the database file).
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    server = spawn_on(data_dir.path(), &owner);
    let a = server.address.clone();
    for (key, body, original) in [
        ("deep-write", &write_body, &write_v1),
        ("deep-read", &read_body, &read_v1),
    ] {
        let (status, replayed) = query(&a, &owner, "ra", body, Some(key));
        assert_eq!(status, 200, "replay {key}: {replayed}");
        assert_eq!(&replayed, original, "replay {key} bytes drifted");
    }
    assert_eq!(count(&a, &owner, "ra"), 1, "replay re-executed the write");
    // The keyed read now reports the old count even though the data moved on.
    let (status, _) = query(
        &a,
        &owner,
        "ra",
        &json!({"sql": "INSERT INTO t(v) VALUES ('later')"}),
        Some("later-1"),
    );
    assert_eq!(status, 200);
    let (status, stale) = query(&a, &owner, "ra", &read_body, Some("deep-read"));
    assert_eq!(status, 200);
    assert_eq!(
        stale, read_v1,
        "keyed read was re-executed instead of replayed"
    );

    // 3. Same key, different body: 409, nothing changes.
    let (status, body) = query(
        &a,
        &owner,
        "ra",
        &json!({"sql": "INSERT INTO t(v) VALUES ('evil')"}),
        Some("deep-write"),
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "idempotency_key_conflict");
    assert_eq!(count(&a, &owner, "ra"), 2);

    // 4. Identical key strings, independent namespaces.
    let shared = json!({"sql": "INSERT INTO t(v) VALUES ('shared')"});
    let (s1, _) = query(&a, &owner, "ra", &shared, Some("shared"));
    let (s2, _) = query(&a, &owner, "rb", &shared, Some("shared"));
    let (s3, app_first) = query(&a, &app, "ra", &shared, Some("shared"));
    assert_eq!((s1, s2, s3), (200, 200, 200));
    let (s4, app_replay) = query(&a, &app, "ra", &shared, Some("shared"));
    assert_eq!(s4, 200);
    assert_eq!(
        app_first, app_replay,
        "cross-principal replay bytes drifted"
    );
    // Namespaces isolate keys, not data: owner-shared and app-shared are two
    // rows in the same table (no 409 across principals, each replays its own).
    assert_eq!(
        count(&a, &owner, "ra"),
        4,
        "cross-namespace write leaked into ra"
    );
    assert_eq!(count(&a, &owner, "rb"), 1, "cross-database write leaked");

    // 2. Fill the owner's ra namespace to the key cap (MAX_REPLAYS in
    // src/execution.rs). Used so far on (owner, ra): ddl-ra, deep-write,
    // deep-read, later-1, shared = 5. All but the last slot are written
    // straight into the replay table -- ten thousand synced requests would
    // make this a disk benchmark -- and the last one is taken through the
    // API, so the cap is reached the way a client reaches it.
    const CAP: usize = 10_000;
    let file = find_file(data_dir.path(), "ra.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&file).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        let principal: String = conn
            .query_row(
                "SELECT principal FROM _dg_replay WHERE key = 'deep-write'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let used: usize = conn
            .query_row(
                "SELECT count(*) FROM _dg_replay WHERE principal = ?",
                [&principal],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize;
        assert_eq!(used, 5);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let tx = conn.transaction().unwrap();
        for i in 0..CAP - used - 1 {
            tx.execute(
                "INSERT INTO _dg_replay VALUES (?, ?, 'h', '{}', ?)",
                rusqlite::params![principal, format!("prefill-{i}"), now],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let (status, body) = query(
        &a,
        &owner,
        "ra",
        &json!({"sql": "INSERT INTO t(v) VALUES ('fill')"}),
        Some("fill-last"),
    );
    assert_eq!(status, 200, "the last free key: {body}");
    let (status, body) = query(
        &a,
        &owner,
        "ra",
        &json!({"sql": "INSERT INTO t(v) VALUES ('over')"}),
        Some("fill-over"),
    );
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded");
    // Existing keys still replay 200 with identical bytes at capacity.
    let (status, replayed) = query(&a, &owner, "ra", &write_body, Some("deep-write"));
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(replayed, write_v1, "replay at cap drifted");
    // Other namespaces are unaffected by (owner, ra) hitting the cap.
    let (status, body) = query(
        &a,
        &owner,
        "rb",
        &json!({"sql": "INSERT INTO t(v) VALUES ('rb-fine')"}),
        Some("rb-after-cap"),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = query(
        &a,
        &app,
        "ra",
        &json!({"sql": "INSERT INTO t(v) VALUES ('app-fine')"}),
        Some("app-after-cap"),
    );
    assert_eq!(status, 200, "{body}");
}

fn spawn_on(data_dir: &std::path::Path, owner: &str) -> Server {
    let _ = owner;
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

/// The one file named `name` under `root`.
fn find_file(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name) {
                return path;
            }
        }
    }
    panic!("{name} not found under {}", root.display());
}
