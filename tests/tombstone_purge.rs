//! Tombstone/purge lifecycle torture (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test tombstone_purge`
//!
//! Five dummy databases are created, filled, and deleted. While
//! tombstoned each name is unqueryable (404), unrecreatable (409), and
//! its file still on disk. Three tombstones get their clock deadline
//! moved into the past (test-fixture-only backdate through
//! metadata.sqlite; production retention is untouched); the idle
//! server maintenance loop must purge exactly those three — files for
//! all suffixes gone, registry rows gone — while the two retained
//! tombstones keep files and keep refusing recreation. The three
//! purged names are then recreated fresh: old rows absent, new rows
//! writable, registry list exact.

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
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
            .timeout(Duration::from_secs(60))
            .send()
            .unwrap();
        (response.status().as_u16(), response.json().unwrap())
    }
    fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (status, response) = self.call(method, path, body);
        assert_eq!(status, 200, "{path}: {response}");
        response
    }
    fn query(&self, name: &str, sql: &str) -> (u16, Value) {
        self.call(
            "POST",
            &format!("/v1/databases/{name}/query"),
            json!({"sql": sql}),
        )
    }
}

fn db_file(server: &Server, name: &str, suffix: &str) -> std::path::PathBuf {
    server.dir.path().join(format!("{name}.sqlite{suffix}"))
}

#[test]
fn tombstoned_names_purge_and_become_reusable() {
    let server = Server::start();
    let live: Vec<String> = (0..5).map(|i| format!("rt_{i}")).collect();
    for name in &live {
        server.ok("POST", "/v1/databases", json!({"name": name}));
        assert_eq!(
            server
                .query(name, "CREATE TABLE d(id INTEGER PRIMARY KEY, v TEXT)")
                .0,
            200
        );
        assert_eq!(
            server
                .query(name, &format!("INSERT INTO d(v) VALUES ('{name}-old')"))
                .0,
            200
        );
    }

    // Delete all five: tombstoned, not gone.
    for name in &live {
        server.ok("DELETE", &format!("/v1/databases/{name}"), json!({}));
        let (status, body) = server.query(name, "SELECT 1");
        assert_eq!(status, 404, "{name} queryable while tombstoned: {body}");
        assert_eq!(body["error"]["code"], "database_not_found");
        let (status, body) = server.call("POST", "/v1/databases", json!({"name": name}));
        assert_eq!(status, 409, "{name} recreatable while tombstoned: {body}");
        assert!(
            db_file(&server, name, "").exists(),
            "{name} file purged early"
        );
    }

    // Backdate only rt_0..rt_2; rt_3/rt_4 stay inside the real window.
    let meta = Connection::open(server.dir.path().join("metadata.sqlite")).unwrap();
    meta.busy_timeout(Duration::from_secs(2)).unwrap();
    let retention: i64 = meta
        .query_row(
            "SELECT purge_after-deleted_at FROM databases WHERE name='rt_0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retention, 86400, "production retention changed under test");
    meta.execute(
        "UPDATE databases SET purge_after=0 WHERE name IN ('rt_0','rt_1','rt_2')",
        [],
    )
    .unwrap();
    drop(meta);

    // Idle server loop purges exactly the three expired tombstones.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let meta = Connection::open(server.dir.path().join("metadata.sqlite")).unwrap();
        let remaining: i64 = meta
            .query_row(
                "SELECT count(*) FROM databases WHERE name IN ('rt_0','rt_1','rt_2')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if remaining == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle server never purged expired tombstones"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    for name in ["rt_0", "rt_1", "rt_2"] {
        for suffix in ["", "-wal", "-shm"] {
            assert!(
                !db_file(&server, name, suffix).exists(),
                "{name}.sqlite{suffix} survived purge"
            );
        }
    }
    for name in ["rt_3", "rt_4"] {
        assert!(
            db_file(&server, name, "").exists(),
            "{name} purged before window"
        );
        let (status, _) = server.call("POST", "/v1/databases", json!({"name": name}));
        assert_eq!(status, 409, "{name} recreatable while retained");
    }

    // Purged names are reusable and fresh: old rows absent, new rows stick.
    for name in ["rt_0", "rt_1", "rt_2"] {
        server.ok("POST", "/v1/databases", json!({"name": name}));
        let (status, body) = server.query(name, "SELECT v FROM d");
        assert_eq!(status, 400, "{name} old table survived purge: {body}");
        assert_eq!(
            server
                .query(name, "CREATE TABLE d(id INTEGER PRIMARY KEY, v TEXT)")
                .0,
            200
        );
        assert_eq!(
            server
                .query(name, &format!("INSERT INTO d(v) VALUES ('{name}-new')"))
                .0,
            200
        );
        let (_, body) = server.query(name, "SELECT v FROM d");
        assert_eq!(body["rows"], json!([[format!("{name}-new")]]));
    }

    // Registry clean: exactly the three live generations.
    assert_eq!(
        server.ok("GET", "/v1/databases", json!({}))["databases"],
        json!(["rt_0", "rt_1", "rt_2"])
    );
}
