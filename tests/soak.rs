//! Soak: concurrent load, migrations and repeated crashes, then a full
//! accounting of every acknowledged write.
//!
//! Run: `timeout 600 cargo test --locked --test soak -- --nocapture`
//! Long: `DG_SOAK_SECS=3600 DG_SOAK_KILL_EVERY=30 cargo test --release --locked --test soak -- --nocapture`
//!
//! For the whole run, writer threads insert rows into several databases
//! (most writes unkeyed `INSERT OR IGNORE` on a unique id, so a retry
//! after an unknown outcome is safe by construction; some writes keyed
//! with `Idempotency-Key` and retried with the same key, which must replay
//! rather than fail), reader threads count rows, and a migrator adds
//! columns with ALTER TABLE. Every few seconds the server is SIGKILLed --
//! no shutdown, mid-write -- and restarted on the same data directory, on
//! a new port the clients pick up.
//!
//! What must hold:
//!
//! * a row count a reader once saw never goes down (an observed commit
//!   is durable across `kill -9`);
//! * a keyed retry after a crash returns success (replayed or executed),
//!   never a conflict or a duplicate;
//! * every acknowledged id is in the database at the end, exactly once,
//!   and nothing is there that was never attempted;
//! * each database's export (the backup path) opens as a SQLite file that
//!   passes `integrity_check` and holds exactly the same ids;
//! * the server never answers 5xx.

use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const DATABASES: usize = 3;
const WRITERS: usize = 6;
const READERS: usize = 3;
/// Keyed writes per database stay well under the documented cap of 10 000 keys
/// per principal per database (docs/api.md), so the cap is not what fails.
const KEYED_PER_DATABASE: u64 = 150;

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Gateway {
    data: PathBuf,
    owner: String,
    address: RwLock<String>,
    child: Mutex<Option<Child>>,
    restarts: AtomicU64,
}

impl Gateway {
    fn start(&self) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(&self.data)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let mut address = None;
        for line in lines.by_ref() {
            let line = line.unwrap();
            if let Ok(log) = serde_json::from_str::<Value>(&line)
                && let Some(addr) = log["fields"]["address"].as_str()
            {
                address = Some(addr.to_string());
                break;
            }
        }
        std::thread::spawn(move || for _ in lines {});
        *self.address.write().unwrap() = address.expect("server never logged its address");
        *self.child.lock().unwrap() = Some(child);
    }

    /// SIGKILL: no shutdown path runs, whatever was in flight is cut.
    fn crash_and_restart(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        self.start();
        self.restarts.fetch_add(1, Ordering::SeqCst);
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Reply {
    Ok(Value),
    /// The server answered with an error code.
    Refused(u16, Value),
    /// No answer: the connection died, so a write's outcome is unknown.
    Unknown,
}

fn send(
    http: &reqwest::blocking::Client,
    gw: &Gateway,
    path: &str,
    body: &Value,
    key: Option<&str>,
) -> Reply {
    let address = gw.address.read().unwrap().clone();
    let mut request = http
        .post(format!("http://{address}{path}"))
        .bearer_auth(&gw.owner)
        .json(body);
    if let Some(key) = key {
        request = request.header("Idempotency-Key", key);
    }
    match request.send() {
        Ok(response) => {
            let status = response.status().as_u16();
            match response.json::<Value>() {
                Ok(body) if status == 200 => Reply::Ok(body),
                Ok(body) => Reply::Refused(status, body),
                Err(_) => Reply::Unknown,
            }
        }
        Err(_) => Reply::Unknown,
    }
}

#[derive(Default)]
struct Ledger {
    /// Ids whose write was acknowledged with 200.
    acked: BTreeSet<String>,
    /// Ids a write was ever sent for.
    attempted: BTreeSet<String>,
    failures: Vec<String>,
    keyed_replays: u64,
    unknown_outcomes: u64,
    busy: u64,
}

fn db(i: usize) -> String {
    format!("soak{i}")
}

#[test]
fn acknowledged_writes_survive_load_migrations_and_repeated_kills() {
    let secs = env_secs("DG_SOAK_SECS", 20);
    let kill_every = env_secs("DG_SOAK_KILL_EVERY", 4);
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let init = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(init.status.success());
    let owner = serde_json::from_slice::<Value>(&init.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let gw = Arc::new(Gateway {
        data,
        owner,
        address: RwLock::new(String::new()),
        child: Mutex::new(None),
        restarts: AtomicU64::new(0),
    });
    gw.start();
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    for i in 0..DATABASES {
        let Reply::Ok(_) = send(&http, &gw, "/v1/databases", &json!({"name": db(i)}), None) else {
            panic!("create {}", db(i));
        };
        let Reply::Ok(_) = send(
            &http,
            &gw,
            &format!("/v1/databases/{}/query", db(i)),
            &json!({"sql": "CREATE TABLE events(id TEXT PRIMARY KEY, writer INTEGER, seq INTEGER, payload TEXT)"}),
            None,
        ) else {
            panic!("schema {}", db(i));
        };
    }

    let ledgers: Arc<Vec<Mutex<Ledger>>> = Arc::new(
        (0..DATABASES)
            .map(|_| Mutex::new(Ledger::default()))
            .collect(),
    );
    let keyed: Arc<Vec<AtomicU64>> = Arc::new((0..DATABASES).map(|_| AtomicU64::new(0)).collect());
    let stop = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut threads = Vec::new();

    for w in 0..WRITERS {
        let (gw, http, ledgers, keyed, stop) = (
            gw.clone(),
            http.clone(),
            ledgers.clone(),
            keyed.clone(),
            stop.clone(),
        );
        threads.push(std::thread::spawn(move || {
            let mut seq = 0u64;
            while !stop.load(Ordering::SeqCst) {
                seq += 1;
                let d = (w + seq as usize) % DATABASES;
                let id = format!("w{w}-{seq}");
                let use_key = keyed[d].fetch_add(1, Ordering::SeqCst) < KEYED_PER_DATABASE
                    && seq.is_multiple_of(3);
                if !use_key {
                    keyed[d].fetch_sub(1, Ordering::SeqCst);
                }
                // A keyed write is a plain INSERT: only the replay protects
                // it from running twice, so a duplicate would surface as a
                // UNIQUE failure on the retry.
                let sql = if use_key {
                    "INSERT INTO events(id, writer, seq, payload) VALUES (?, ?, ?, ?)"
                } else {
                    "INSERT OR IGNORE INTO events(id, writer, seq, payload) VALUES (?, ?, ?, ?)"
                };
                let body =
                    json!({"sql": sql, "args": [id, w, seq, "x".repeat((seq % 200) as usize)]});
                let key = use_key.then(|| format!("k-{id}"));
                ledgers[d].lock().unwrap().attempted.insert(id.clone());
                let path = format!("/v1/databases/{}/query", db(d));
                let mut first = true;
                loop {
                    match send(&http, &gw, &path, &body, key.as_deref()) {
                        Reply::Ok(_) => {
                            let mut ledger = ledgers[d].lock().unwrap();
                            if !first && key.is_some() {
                                ledger.keyed_replays += 1;
                            }
                            ledger.acked.insert(id.clone());
                            break;
                        }
                        Reply::Refused(429, _) | Reply::Refused(408, _) => {
                            ledgers[d].lock().unwrap().busy += 1;
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Reply::Refused(status, response) => {
                            ledgers[d].lock().unwrap().failures.push(format!(
                                "write {id} (keyed: {}, retry: {}): HTTP {status} {response}",
                                key.is_some(),
                                !first
                            ));
                            break;
                        }
                        Reply::Unknown => {
                            // Outcome unknown: the server is down or died
                            // mid-request. The write is safe to resend --
                            // by its key, or by INSERT OR IGNORE on its id.
                            ledgers[d].lock().unwrap().unknown_outcomes += 1;
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                    first = false;
                }
            }
        }));
    }

    // Readers: an observed count is a promise. It must never shrink.
    let high_water: Arc<Vec<AtomicU64>> =
        Arc::new((0..DATABASES).map(|_| AtomicU64::new(0)).collect());
    for r in 0..READERS {
        let (gw, http, ledgers, high_water, stop) = (
            gw.clone(),
            http.clone(),
            ledgers.clone(),
            high_water.clone(),
            stop.clone(),
        );
        threads.push(std::thread::spawn(move || {
            let mut i = r;
            while !stop.load(Ordering::SeqCst) {
                i += 1;
                let d = i % DATABASES;
                let path = format!("/v1/databases/{}/query", db(d));
                match send(&http, &gw, &path, &json!({"sql": "SELECT count(*) FROM events"}), None) {
                    Reply::Ok(body) => {
                        let count = body["rows"][0][0].as_u64().unwrap();
                        let before = high_water[d].fetch_max(count, Ordering::SeqCst);
                        if count < before {
                            ledgers[d].lock().unwrap().failures.push(format!(
                                "{}: row count went down from {before} to {count}: an observed commit was lost",
                                db(d)
                            ));
                        }
                    }
                    Reply::Refused(status, body) if status >= 500 => {
                        ledgers[d].lock().unwrap().failures.push(format!("read: HTTP {status} {body}"));
                    }
                    _ => {}
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }));
    }

    // Migrator: schema changes under load.
    {
        let (gw, http, ledgers, stop) = (gw.clone(), http.clone(), ledgers.clone(), stop.clone());
        threads.push(std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(700));
                n += 1;
                let d = n % DATABASES;
                let sql = format!("ALTER TABLE events ADD COLUMN extra{n} TEXT DEFAULT 'm{n}'");
                let path = format!("/v1/databases/{}/query", db(d));
                // Unkeyed DDL with an unknown outcome is checked by asking
                // SQLite whether the column is there; a retry would fail
                // with "duplicate column" if the first attempt landed.
                loop {
                    match send(&http, &gw, &path, &json!({"sql": sql}), None) {
                        Reply::Ok(_) => break,
                        Reply::Refused(429 | 408, _) => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        Reply::Refused(400, body)
                            if body.to_string().contains("duplicate column")
                                || body["error"]["code"] == "invalid_sql" =>
                        {
                            break;
                        }
                        Reply::Refused(status, body) => {
                            ledgers[d]
                                .lock()
                                .unwrap()
                                .failures
                                .push(format!("{sql}: HTTP {status} {body}"));
                            break;
                        }
                        Reply::Unknown => std::thread::sleep(Duration::from_millis(50)),
                    }
                }
            }
        }));
    }

    // Chaos: kill -9 and restart, on a fixed rhythm, until the deadline.
    while Instant::now() < deadline {
        std::thread::sleep(
            Duration::from_secs(kill_every).min(deadline.saturating_duration_since(Instant::now())),
        );
        if Instant::now() < deadline {
            gw.crash_and_restart();
        }
    }
    stop.store(true, Ordering::SeqCst);
    for t in threads {
        t.join().unwrap();
    }
    // One last crash with nothing in flight, so the accounting below reads
    // what recovery left on disk, not a warm process's memory.
    gw.crash_and_restart();

    let mut report = BTreeMap::new();
    let mut failures = Vec::new();
    for d in 0..DATABASES {
        let ledger = ledgers[d].lock().unwrap();
        failures.extend(ledger.failures.iter().cloned());
        let path = format!("/v1/databases/{}/query", db(d));
        // Paged: a long run holds more rows than one response may carry.
        let mut rows: Vec<String> = Vec::new();
        loop {
            let page = json!({"sql": "SELECT id FROM events ORDER BY id LIMIT 500 OFFSET ?", "args": [rows.len()]});
            let body = match send(&http, &gw, &path, &page, None) {
                Reply::Ok(body) => body,
                Reply::Refused(status, body) => {
                    panic!("final read of {}: HTTP {status} {body}", db(d))
                }
                Reply::Unknown => panic!("final read of {}: no answer", db(d)),
            };
            let got = body["rows"].as_array().unwrap();
            rows.extend(got.iter().map(|r| r[0].as_str().unwrap().to_string()));
            if got.len() < 500 {
                break;
            }
        }
        let present: BTreeSet<String> = rows.iter().cloned().collect();
        if present.len() != rows.len() {
            failures.push(format!("{}: duplicate ids", db(d)));
        }
        let lost: Vec<_> = ledger.acked.difference(&present).take(5).collect();
        if !lost.is_empty() {
            failures.push(format!(
                "{}: acknowledged writes missing after recovery: {lost:?}",
                db(d)
            ));
        }
        let invented: Vec<_> = present.difference(&ledger.attempted).take(5).collect();
        if !invented.is_empty() {
            failures.push(format!(
                "{}: rows that were never written: {invented:?}",
                db(d)
            ));
        }

        // The backup path: export, open as a plain SQLite file, check it.
        let Reply::Ok(export) = send(
            &http,
            &gw,
            &format!("/v1/databases/{}/export", db(d)),
            &json!({}),
            None,
        ) else {
            panic!("export {}", db(d));
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(export["data_base64"].as_str().unwrap())
            .unwrap();
        let file = dir.path().join(format!("{}.sqlite", db(d)));
        std::fs::write(&file, bytes).unwrap();
        let conn = rusqlite::Connection::open(&file).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        if integrity != "ok" {
            failures.push(format!("{}: export integrity_check: {integrity}", db(d)));
        }
        let exported: BTreeSet<String> = conn
            .prepare("SELECT id FROM events")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        if exported != present {
            failures.push(format!("{}: export differs from the live database", db(d)));
        }
        report.insert(
            db(d),
            json!({
                "rows": present.len(),
                "acked": ledger.acked.len(),
                "attempted": ledger.attempted.len(),
                "keyed_replays_after_crash": ledger.keyed_replays,
                "unknown_outcomes_retried": ledger.unknown_outcomes,
                "busy_retries": ledger.busy,
                "highest_count_seen": high_water[d].load(Ordering::SeqCst),
            }),
        );
    }
    eprintln!(
        "soak: {secs}s, {} kill -9 restarts\n{}",
        gw.restarts.load(Ordering::SeqCst),
        serde_json::to_string_pretty(&report).unwrap()
    );
    assert!(
        failures.is_empty(),
        "soak failures:\n  {}",
        failures.join("\n  ")
    );
    // A run in which the crashes never landed on a write tests nothing.
    let unknown: u64 = report
        .values()
        .map(|r| r["unknown_outcomes_retried"].as_u64().unwrap())
        .sum();
    assert!(
        unknown > 0 || secs < kill_every,
        "no write was ever cut by a crash"
    );
}
