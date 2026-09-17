//! Lifecycle collision and idle-retention regression tests against a real server.
use base64::Engine;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
    time::{Duration, Instant},
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
        // Keep draining: maintenance diagnostics must never fill the pipe.
        std::thread::spawn(move || for _ in lines {});
        Self {
            child,
            dir,
            url: format!("http://{address}"),
            owner,
        }
    }
    fn call(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let response = reqwest::blocking::Client::new()
            .request(method.parse().unwrap(), format!("{}{path}", self.url))
            .bearer_auth(&self.owner)
            .json(&body)
            .timeout(Duration::from_secs(15))
            .send()
            .unwrap();
        (response.status().as_u16(), response.json().unwrap())
    }
    fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (status, response) = self.call(method, path, body);
        assert_eq!(status, 200, "{path}: {response}");
        response
    }
    fn query(&self, name: &str, sql: &str) -> Value {
        self.ok(
            "POST",
            &format!("/v1/databases/{name}/query"),
            json!({"sql":sql}),
        )
    }
}

fn independent_snapshot(bytes: &[u8], expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.sqlite");
    std::fs::write(&path, bytes).unwrap();
    let conn = Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        conn.query_row("SELECT v FROM marker", [], |r| r.get::<_, String>(0))
            .unwrap(),
        expected
    );
}

#[test]
fn concurrent_same_source_exports_and_same_target_publication_are_isolated() {
    let server = Server::start();
    server.ok("POST", "/v1/databases", json!({"name":"source"}));
    server.query("source", "CREATE TABLE marker(v TEXT)");
    server.query("source", "INSERT INTO marker VALUES ('source')");
    // Multiple backup steps make same-source overlap likely; deterministic
    // file-ownership overlap is separately asserted in the store unit test.
    server.query("source", "CREATE TABLE padding(v BLOB)");
    server.query("source", "INSERT INTO padding VALUES (zeroblob(1000000))");
    let exports = std::thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let server = &server;
                scope.spawn(move || {
                    barrier.wait();
                    server.ok("POST", "/v1/databases/source/export", json!({}))
                })
            })
            .collect();
        barrier.wait();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    for export in &exports {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(export["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(export["size"], bytes.len());
        independent_snapshot(&bytes, "source");
    }
    // Competing clones: exactly one atomic publication, no overwritten winner.
    for (target, import) in [("clone_target", false), ("import_target", true)] {
        let results = std::thread::scope(|scope| {
            let barrier = Arc::new(Barrier::new(3));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    let server = &server;
                    let export = &exports[0];
                    scope.spawn(move || {
                        barrier.wait();
                        if import {
                            server.call(
                                "POST",
                                "/v1/databases/source/import",
                                json!({"name":target,"data_base64":export["data_base64"]}),
                            )
                        } else {
                            server.call(
                                "POST",
                                "/v1/databases/source/clone",
                                json!({"destination":target}),
                            )
                        }
                    })
                })
                .collect();
            barrier.wait();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().0)
                .collect::<Vec<_>>()
        });
        let mut statuses = results;
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
        assert_eq!(
            server.query(target, "SELECT v FROM marker")["rows"],
            json!([["source"]])
        );
    }
    assert!(std::fs::read_dir(server.dir.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".lifecycle-")
    }));
}

#[test]
fn idle_server_purges_only_expired_tombstones_and_allows_safe_recreation() {
    let server = Server::start();
    server.ok("POST", "/v1/databases", json!({"name":"expired"}));
    server.ok("POST", "/v1/databases", json!({"name":"retained"}));
    server.ok("DELETE", "/v1/databases/expired", json!({}));
    server.ok("DELETE", "/v1/databases/retained", json!({}));
    let meta = Connection::open(server.dir.path().join("metadata.sqlite")).unwrap();
    meta.busy_timeout(Duration::from_secs(2)).unwrap();
    let retention: i64 = meta
        .query_row(
            "SELECT purge_after-deleted_at FROM databases WHERE name='expired'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retention, 86400);
    // Move only this test fixture's clock deadline into the past. There is no
    // production retention override and no lifecycle request to trigger purge.
    meta.execute(
        "UPDATE databases SET purge_after=0 WHERE name='expired'",
        [],
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining: i64 = meta
            .query_row(
                "SELECT count(*) FROM databases WHERE name='expired'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if remaining == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle server never purged expired tombstone"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    for suffix in ["", "-wal", "-shm"] {
        assert!(
            !server
                .dir
                .path()
                .join(format!("expired.sqlite{suffix}"))
                .exists()
        );
    }
    assert!(server.dir.path().join("retained.sqlite").exists());
    server.ok("POST", "/v1/databases", json!({"name":"expired"}));
    server.query("expired", "CREATE TABLE marker(v TEXT)");
    server.query("expired", "INSERT INTO marker VALUES ('new_generation')");
    assert_eq!(
        server.query("expired", "SELECT v FROM marker")["rows"],
        json!([["new_generation"]])
    );
    assert_eq!(
        server.ok("GET", "/v1/databases", json!({}))["databases"],
        json!(["expired"])
    );
}

#[test]
fn imported_external_sqlite_file_supports_m1_keyed_writes() {
    let server = Server::start();
    // An ordinary SQLite database, produced by plain rusqlite with no gateway
    // involvement: exactly what an owner would import from elsewhere.
    let dir = tempfile::tempdir().unwrap();
    let external = dir.path().join("external.sqlite");
    let conn = Connection::open(&external).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE;
         CREATE TABLE ledger (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO ledger VALUES (1, 'original');",
    )
    .unwrap();
    drop(conn);
    let encoded = base64::engine::general_purpose::STANDARD.encode(std::fs::read(&external).unwrap());
    server.ok(
        "POST",
        "/v1/databases/external/import",
        json!({"name": "imported", "data_base64": encoded}),
    );
    // Plain read works, then a keyed write succeeds exactly once.
    assert_eq!(server.query("imported", "SELECT value FROM ledger")["rows"], json!([["original"]]));
    let write = json!({"sql": "UPDATE ledger SET value='spent' WHERE id=1"});
    let first = server.call("POST", "/v1/databases/imported/query", write.clone());
    assert_eq!(first.0, 200, "{}", first.1);
    let replay = server.call("POST", "/v1/databases/imported/query", write.clone());
    assert_eq!(replay.0, 200, "{}", replay.1);
    assert_eq!(replay.1, first.1, "keyed replay must return the stored result");
    assert_eq!(
        server.query("imported", "SELECT count(*) FROM ledger WHERE value='spent'")["rows"],
        json!([[1]]),
        "exactly one committed mutation despite the second request"
    );
    // The published file is a full gateway database with the replay schema.
    let store = dg::store::Store::open(server.dir.path()).unwrap();
    let path = store.path("imported").unwrap();
    let replay_tables: i64 = Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM sqlite_master WHERE name='_dg_replay'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(replay_tables, 1);
}
