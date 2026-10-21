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
        self.call_keyed(method, path, body, None)
    }
    /// The server's admission counters for one database: how many
    /// attempts it has counted, and how many of them were shed.
    ///
    /// The status endpoint answers from the metrics and never takes the
    /// database's guard, so it answers even while a request holds it.
    fn queue_counters(&self, name: &str) -> (u64, u64) {
        let (status, body) = self.call("GET", "/v1/status", json!({}));
        assert_eq!(status, 200, "status: {body}");
        let entry = &body["databases"][name];
        (
            entry["queue_waits"].as_u64().unwrap_or(0),
            entry["queue_timeouts"].as_u64().unwrap_or(0),
        )
    }
    fn call_keyed(
        &self,
        method: &str,
        path: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> (u16, Value) {
        let mut request = reqwest::blocking::Client::new()
            .request(method.parse().unwrap(), format!("{}{path}", self.url))
            .bearer_auth(&self.owner)
            .json(&body)
            .timeout(Duration::from_secs(60));
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().unwrap();
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

/// A 429 means the request was shed before lifecycle work began, so retrying
/// that request is safe. Retrying here keeps the publication assertion about
/// atomic conflict (409) independent of unrelated machine load.
fn call_publication_retry_shed(
    server: &Server,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, response) = server.call(method, path, body.clone());
        if status != 429 {
            return (status, response);
        }
        assert_eq!(response["error"]["code"], "capacity_exceeded", "{response}");
        assert!(
            Instant::now() < deadline,
            "publication request stayed shed: {response}"
        );
        std::thread::sleep(Duration::from_millis(25));
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
                            call_publication_retry_shed(
                                server,
                                "POST",
                                "/v1/databases/source/import",
                                json!({"name":target,"data_base64":export["data_base64"]}),
                            )
                        } else {
                            call_publication_retry_shed(
                                server,
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
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(std::fs::read(&external).unwrap());
    server.ok(
        "POST",
        "/v1/databases/external/import",
        json!({"name": "imported", "data_base64": encoded}),
    );
    // Plain read works, then a keyed INSERT executes exactly once: the replay
    // returns the stored original response and the second execution is skipped.
    assert_eq!(
        server.query("imported", "SELECT value FROM ledger")["rows"],
        json!([["original"]])
    );
    let write = json!({"sql": "INSERT INTO ledger VALUES (2, 'keyed')"});
    let first = server.call_keyed(
        "POST",
        "/v1/databases/imported/query",
        write.clone(),
        Some("imported-keyed-write"),
    );
    assert_eq!(first.0, 200, "{}", first.1);
    let replay = server.call_keyed(
        "POST",
        "/v1/databases/imported/query",
        write.clone(),
        Some("imported-keyed-write"),
    );
    assert_eq!(replay.0, 200, "{}", replay.1);
    assert_eq!(
        replay.1, first.1,
        "keyed replay must return the stored result"
    );
    assert_eq!(
        server.query("imported", "SELECT count(*) FROM ledger WHERE id=2")["rows"],
        json!([[1]]),
        "exactly one committed mutation despite the second keyed request"
    );
    // The published file is a full gateway database with the replay schema.
    let store = dg::store::Store::open(server.dir.path()).unwrap();
    let path = store.path("imported").unwrap();
    let replay_tables: i64 = Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='_dg_replay'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(replay_tables, 1);
}

/// Wait until the server counts an ADMITTED request for `name`.
///
/// The wait counter moves for every attempt, including the ones the queue
/// sheds, so the count alone does not say the holder is in. The timeout
/// counter is what tells them apart: an admitted attempt raises the waits
/// and not the timeouts.
///
/// The probe must not start before this returns. The probe is a flood of
/// reads on the holder's own database, and reads share the guard with
/// each other, so a probe that starts first can keep the read batch busy
/// enough that the holder's writer never gets its turn — the
/// writer-starvation gap the design documents.
fn wait_for_admission(server: &Server, name: &str, baseline: (u64, u64)) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (waits, timeouts) = server.queue_counters(name);
        let st = server.call("GET", "/v1/status", json!({})).1;
        println!("DIAGWAIT {name} waits={waits} timeouts={timeouts} base={baseline:?} admission={}", st["admission"]);
        if waits > baseline.0 && timeouts == baseline.1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the holder on {name} never took the guard"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll a quick read until the server really rejects it: the observed 429 is
/// the contention signal, so no sleep-based overlap is assumed. Returns after
/// the first rejection or panics past a bounded wait.
fn assert_observes_429(server: &Server, name: &str) {
    assert_observes_429_with(server, name, "SELECT 1");
}
/// Same, with a caller-chosen probe statement. A write probe contends for
/// the write lane; a read probe contends for the read lane and the
/// per-database guard.
fn assert_observes_429_with(server: &Server, name: &str, sql: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, body) = server.call(
            "POST",
            &format!("/v1/databases/{name}/query"),
            json!({"sql": sql}),
        );
        println!("DIAGPROBE status={status}");
        if status == 429 {
            assert_eq!(body["error"]["code"], "capacity_exceeded", "{body}");
            return;
        }
        assert_eq!(
            status, 200,
            "unexpected status while awaiting contention: {body}"
        );
        assert!(
            Instant::now() < deadline,
            "holders never overlapped; no 429 observed"
        );
    }
}

const SLOW_INSERT: &str =
    "INSERT INTO t WITH RECURSIVE c(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM c) SELECT v FROM c";

/// Send the slow insert until the server admits it (any non-429), bounded.
/// A thread-start barrier cannot synchronize server-side permit acquisition:
/// a racing poller may win the permit first, so a single holder attempt can
/// be rejected while no contention window ever exists. Retrying until
/// admission guarantees the holder actually holds its permit for ~2s, during
/// which the polling side must observe a 429.
fn slow_insert_until_admitted(server: &Server, name: &str) -> (u16, Value) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = server.call(
            "POST",
            &format!("/v1/databases/{name}/query"),
            json!({"sql": SLOW_INSERT}),
        );
        println!("DIAGHOLDER {name} status={} body={}", result.0, result.1);
        if result.0 != 429 {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "slow holder never admitted past 20s of 429s"
        );
    }
}

fn assert_holder_hit_deadline(result: (u16, Value)) {
    assert_eq!(
        result.0, 408,
        "holder must run into the deadline: {}",
        result.1
    );
    assert_eq!(result.1["error"]["code"], "query_deadline_exceeded");
}

#[test]
fn contended_capacity_rejects_with_429_and_server_recovers() {
    let server = Server::start();
    for name in ["hot", "g0", "g1", "g2", "g3", "probe"] {
        server.ok("POST", "/v1/databases", json!({"name": name}));
        server.query(name, "CREATE TABLE t (v INTEGER)");
    }
    // Phase 1: one slow query holds the per-database queue (size 1); a
    // concurrent read on the same database must see 429 capacity_exceeded.
    std::thread::scope(|scope| {
        let server = &server;
        let baseline = server.queue_counters("hot");
        let holder = scope.spawn(move || slow_insert_until_admitted(server, "hot"));
        wait_for_admission(server, "hot", baseline);
        assert_observes_429(server, "hot");
        assert_holder_hit_deadline(holder.join().unwrap());
    });
    // Phase 2: four slow queries on distinct databases hold all four
    // write-lane permits; another *write* must see 429 capacity_exceeded.
    // Reads intentionally do NOT shed here — they share the wide read lane
    // (scale proposal Phase 2) — so the probe is an INSERT into the scratch
    // database, which carries no assertions.
    std::thread::scope(|scope| {
        let server = &server;
        let baselines: Vec<(u64, u64)> = ["g0", "g1", "g2", "g3"]
            .iter()
            .map(|name| server.queue_counters(name))
            .collect();
        let holders: Vec<_> = ["g0", "g1", "g2", "g3"]
            .into_iter()
            .map(|name| scope.spawn(move || slow_insert_until_admitted(server, name)))
            .collect();
        for (name, baseline) in ["g0", "g1", "g2", "g3"].iter().zip(baselines) {
            wait_for_admission(server, name, baseline);
        }
        assert_observes_429_with(server, "probe", "INSERT INTO t VALUES (1)");
        for holder in holders {
            assert_holder_hit_deadline(holder.join().unwrap());
        }
    });
    // Post-contention recovery: normal writes and exact reads work again, and
    // the interrupted inserts left no partial rows anywhere.
    server.query("hot", "INSERT INTO t VALUES (1)");
    assert_eq!(server.query("hot", "SELECT v FROM t")["rows"], json!([[1]]));
    for name in ["g0", "g1", "g2", "g3"] {
        assert_eq!(
            server.query(name, "SELECT count(*) FROM t")["rows"],
            json!([[0]]),
            "{name} holds partial rows from an interrupted insert"
        );
    }
}
