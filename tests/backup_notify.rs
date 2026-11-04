//! Backup-health notification delivery, end to end through the real binary
//! (observability contract §3 alert 4, owner's 2026-09-27 decision).
//!
//! Run: `timeout 600 cargo test --locked --test backup_notify`
//!
//! Delivery is the half that was missing: `GET /v1/backups` already answered
//! "how fresh is `beta`" for someone who asked, and nobody asks at 03:00. So
//! this drives a real `dg serve` with a short cadence, points the webhook sink
//! at a collector this test owns, and asserts on what actually arrived:
//!
//!   * the daily digest arrives on its cadence, with per-database state, the
//!     newest verified set, an age, and a bounded operator action;
//!   * a failed attempt recorded by the producer produces an IMMEDIATE notice
//!     naming that attempt and the step it reached;
//!   * that notice arrives ONCE. A failure poll that re-announced on every
//!     tick would turn one failure into 1,440 alerts a day, which is how
//!     operators learn to ignore alerts;
//!   * a later, distinct failure alerts again, and a success does not;
//!   * a success still leaves the digest reporting `fresh` for that database.
//!
//! The collector is a raw loopback socket rather than an axum server: the
//! point is to observe bytes crossing the process boundary, and a second
//! server in the test would add moving parts without adding proof.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

/// A loopback HTTP collector. Records the JSON body of every POST, and
/// answers 200 so the sink sees a healthy delivery.
struct Collector {
    address: String,
    notes: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    /// Set when the test wants the sink to be told the delivery failed.
    reject: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Collector {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let notes = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let reject = Arc::new(AtomicBool::new(false));
        let (sink_notes, sink_stop, sink_reject) = (notes.clone(), stop.clone(), reject.clone());
        let thread = std::thread::spawn(move || {
            while !sink_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut content_length = 0usize;
                        let mut line = String::new();
                        while reader.read_line(&mut line).unwrap_or(0) > 0 {
                            let trimmed = line.trim_end().to_string();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some(value) = trimmed
                                .split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            {
                                content_length = value;
                            }
                            line.clear();
                        }
                        let mut body = vec![0u8; content_length];
                        if reader.read_exact(&mut body).is_ok()
                            && let Ok(value) = serde_json::from_slice::<Value>(&body)
                        {
                            sink_notes.lock().unwrap().push(value);
                        }
                        let status = if sink_reject.load(Ordering::SeqCst) {
                            "HTTP/1.1 500 Internal Server Error"
                        } else {
                            "HTTP/1.1 200 OK"
                        };
                        let _ = stream.write_all(
                            format!("{status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                                .as_bytes(),
                        );
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            notes,
            stop,
            reject,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/hook", self.address)
    }

    fn of_kind(&self, kind: &str) -> Vec<Value> {
        self.notes
            .lock()
            .unwrap()
            .iter()
            .filter(|note| note["kind"] == kind)
            .cloned()
            .collect()
    }

    /// Wait for a note of `kind` that satisfies `ready`, so a test never
    /// asserts on a delivery that has not landed yet -- and never sleeps for
    /// a fixed period hoping one did.
    fn wait_for(&self, kind: &str, timeout: Duration, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(note) = self.of_kind(kind).into_iter().find(|note| ready(note)) {
                return note;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "no {kind} note within {timeout:?}; saw {:?}",
            self.notes.lock().unwrap()
        );
    }

    fn count(&self, kind: &str) -> usize {
        self.of_kind(kind).len()
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Server {
    child: Child,
    address: String,
    /// The stderr drain. Held so it outlives the handshake, and kept running
    /// for the life of the server: a digest cadence of one second fills a
    /// stderr pipe nobody reads within a minute, and a server blocked writing
    /// a log line looks exactly like a notifier that never fires.
    _stderr: std::thread::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn init(data_dir: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string()
}

fn start(data_dir: &std::path::Path, extra: &[&str]) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dg"));
    cmd.args([
        "serve",
        "--listen",
        "127.0.0.1:0",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    cmd.args(extra);
    let mut child = cmd.stderr(Stdio::piped()).spawn().unwrap();
    let stderr = child.stderr.take().unwrap();
    // Drain continuously in the background. Reading only the first line and
    // dropping the reader would leave a live server writing into a pipe
    // nobody drains: at a one-second digest cadence that pipe fills within a
    // minute and the server blocks mid-notification.
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(log) = serde_json::from_str::<Value>(&line)
                && let Some(address) = log.pointer("/fields/address").and_then(|v| v.as_str())
            {
                let _ = tx.send(address.to_string());
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server logged its listen address");
    Server {
        child,
        address,
        _stderr: drain,
    }
}

fn post(address: &str, path: &str, cred: &str, body: Value) -> (u16, Value) {
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(format!("http://{address}{path}"))
        .bearer_auth(cred)
        .json(&body)
        .timeout(Duration::from_secs(60))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().unwrap_or(Value::Null))
}

fn create(address: &str, cred: &str, name: &str) {
    let (status, body) = post(address, "/v1/databases", cred, json!({"name": name}));
    assert_eq!(status, 200, "create {name}: {body}");
}

/// Append one attempt record, exactly the shape the producer
/// (`scripts/backup.sh`) writes to `$BACKUP_ROOT/attempts.jsonl`.
fn record_attempt(root: &std::path::Path, ts: &str, outcome: &str, step: &str, set: Option<&str>) {
    let record = json!({
        "ts": ts,
        "started_at": 1_700_000_000u64,
        "completed_at": 1_700_000_010u64,
        "outcome": outcome,
        "set": set,
        "databases": 0,
        "step": step,
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("attempts.jsonl"))
        .unwrap();
    writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
    file.sync_all().unwrap();
}

/// A versioned set whose coverage boundary is `coverage_start`, so a test can
/// make a database provably fresh by publishing a boundary after its last
/// write.
fn write_set(root: &std::path::Path, stamp: &str, databases: &[&str], coverage_start: u64) {
    let set = root.join(stamp);
    fs::create_dir_all(&set).unwrap();
    fs::write(
        set.join("manifest.json"),
        serde_json::to_vec(&json!({
            "started_at": coverage_start,
            "completed_at": coverage_start + 1,
            "registry_captured_at": coverage_start + 1,
            "databases": databases.iter().map(|name| json!({
                "name": name,
                "file": format!("{name}.sqlite"),
                "size": 1,
                "sha256": "0".repeat(64),
            })).collect::<Vec<_>>(),
            "registry": {"file": "metadata.sqlite.bak", "size": 1, "sha256": "0".repeat(64)},
        }))
        .unwrap(),
    )
    .unwrap();
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn the_digest_is_delivered_on_its_cadence_with_what_an_operator_needs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("backups");
    fs::create_dir_all(&root).unwrap();
    let cred = init(&data);
    let collector = Collector::start();
    let _server = start(
        &data,
        &[
            "--backup-root",
            root.to_str().unwrap(),
            // Seconds rather than a day: the cadence under test, not a wall
            // clock. A test that waited 24h would prove nothing a 1s tick
            // does not, and would never finish.
            "--backup-digest-secs",
            "1",
            "--backup-failure-poll-secs",
            "1",
            "--backup-notify-webhook",
            &collector.url(),
        ],
    );
    create(&_server.address, &cred, "alpha");

    let note = collector.wait_for("backup_digest", Duration::from_secs(30), |note| {
        note["databases"]
            .as_array()
            .is_some_and(|dbs| dbs.iter().any(|db| db["name"] == "alpha"))
    });
    // Contract section 3: the database, and the state that says it is not
    // covered. With no set at all the only honest answer is unknown.
    let alpha = note["databases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|db| db["name"] == "alpha")
        .unwrap();
    assert_eq!(alpha["state"], "unknown");
    // A bounded operator action, and no claim of data-loss prevention.
    let action = note["action"].as_str().unwrap();
    assert!(action.contains("dg backup"), "unhelpful action: {action}");
    for forbidden in ["rpo", "rto", "no data loss", "recovered"] {
        assert!(
            !action.to_lowercase().contains(forbidden),
            "digest claims {forbidden}: {action}"
        );
    }
    // A fresh database arrives with the set that proved it and an age, which
    // is what makes the digest actionable rather than decorative.
    write_set(&root, "20240101T000000Z", &["alpha"], now_secs() + 60);
    let fresh = collector.wait_for("backup_digest", Duration::from_secs(30), |note| {
        note["databases"].as_array().is_some_and(|dbs| {
            dbs.iter()
                .any(|db| db["name"] == "alpha" && db["state"] == "fresh")
        })
    });
    let alpha = fresh["databases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|db| db["name"] == "alpha")
        .unwrap();
    assert_eq!(alpha["newest_set"], "20240101T000000Z");
    assert!(alpha["age_secs"].is_number(), "a fresh row carries an age");
}

#[test]
fn a_failed_attempt_is_notified_immediately_and_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("backups");
    fs::create_dir_all(&root).unwrap();
    let cred = init(&data);
    let collector = Collector::start();
    let _server = start(
        &data,
        &[
            "--backup-root",
            root.to_str().unwrap(),
            "--backup-digest-secs",
            "3600",
            "--backup-failure-poll-secs",
            "1",
            "--backup-notify-webhook",
            &collector.url(),
        ],
    );
    create(&_server.address, &cred, "alpha");

    // The digest cadence is an hour here, so anything that arrives inside a
    // few seconds can only be the immediate failure path.
    record_attempt(&root, "20240101T000000Z", "error", "manifest", None);
    let note = collector.wait_for("backup_failure", Duration::from_secs(30), |_| true);
    assert_eq!(note["severity"], "alert");
    let failure = note["failure"].as_object().expect("failure record travels");
    assert_eq!(failure["ts"], "20240101T000000Z");
    assert_eq!(failure["step"], "manifest");
    assert!(
        note["title"]
            .as_str()
            .unwrap()
            .contains("failed at step manifest"),
        "title must name the step: {}",
        note["title"]
    );
    assert!(
        note["action"]
            .as_str()
            .unwrap()
            .contains("dg backup verify")
    );

    // The de-duplication contract: one failure, one alert, however many
    // times the poll runs afterwards.
    let after_first = collector.count("backup_failure");
    std::thread::sleep(Duration::from_millis(3_500));
    assert_eq!(
        collector.count("backup_failure"),
        after_first,
        "a poll must not re-announce an attempt it already reported"
    );
    assert!(after_first >= 1);

    // A different failed attempt is different news, and must alert.
    record_attempt(&root, "20240102T000000Z", "error", "publish", None);
    collector.wait_for("backup_failure", Duration::from_secs(30), |note| {
        note["failure"]["ts"] == "20240102T000000Z"
    });
    assert_eq!(collector.count("backup_failure"), after_first + 1);

    // A success afterwards is not a failure and must not alert...
    record_attempt(
        &root,
        "20240103T000000Z",
        "ok",
        "publish",
        Some("20240103T000000Z"),
    );
    std::thread::sleep(Duration::from_millis(3_500));
    assert_eq!(collector.count("backup_failure"), after_first + 1);

    // ...and it must not silence a later failure either.
    record_attempt(&root, "20240104T000000Z", "error", "fsync", None);
    collector.wait_for("backup_failure", Duration::from_secs(30), |note| {
        note["failure"]["ts"] == "20240104T000000Z"
    });
}

#[test]
fn a_sink_that_rejects_does_not_stop_the_next_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("backups");
    fs::create_dir_all(&root).unwrap();
    let cred = init(&data);
    let collector = Collector::start();
    // The collector counts what arrived before it started rejecting, so the
    // test can prove both that the failure was seen and that delivery
    // resumed.
    let _server = start(
        &data,
        &[
            "--backup-root",
            root.to_str().unwrap(),
            "--backup-digest-secs",
            "1",
            "--backup-failure-poll-secs",
            "0",
            "--backup-notify-webhook",
            &collector.url(),
        ],
    );
    create(&_server.address, &cred, "alpha");
    collector.wait_for("backup_digest", Duration::from_secs(30), |note| {
        note["databases"]
            .as_array()
            .is_some_and(|dbs| dbs.iter().any(|db| db["name"] == "alpha"))
    });
    collector.reject.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(2_500));
    collector.reject.store(false, Ordering::SeqCst);
    let before = collector.count("backup_digest");
    // A 500-ing sink is reported, never retried into a storm, and never
    // allowed to wedge the loop: the next digest still goes out.
    //
    // The wait is for the COUNT to grow, not for a digest note to exist:
    // the collector already holds the digests from before the sink
    // started rejecting, so waiting for a note returns at once and the
    // assertion that follows would race the next delivery instead of
    // waiting for it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while collector.count("backup_digest") <= before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        collector.count("backup_digest") > before,
        "delivery did not resume after the sink recovered"
    );
}

#[test]
fn delivery_is_off_unless_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let cred = init(&data);
    let collector = Collector::start();
    // No notification flags at all: nothing is delivered anywhere, and the
    // log sink alone is what an unconfigured gateway uses.
    let _server = start(&data, &[]);
    create(&_server.address, &cred, "alpha");
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(
        collector.count("backup_digest"),
        0,
        "an unconfigured gateway must not notify anyone"
    );
}

#[test]
fn dg_backup_digest_reports_the_same_thing_the_sink_sends_and_works_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("backups");
    fs::create_dir_all(&root).unwrap();
    let cred = init(&data);
    let collector = Collector::start();
    let server = start(
        &data,
        &[
            "--backup-root",
            root.to_str().unwrap(),
            "--backup-digest-secs",
            "1",
            "--backup-failure-poll-secs",
            "0",
            "--backup-notify-webhook",
            &collector.url(),
        ],
    );
    create(&server.address, &cred, "alpha");
    let delivered = collector.wait_for("backup_digest", Duration::from_secs(30), |note| {
        note["databases"]
            .as_array()
            .is_some_and(|dbs| dbs.iter().any(|db| db["name"] == "alpha"))
    });

    // The digest an operator reads on demand comes from the same rule, so it
    // cannot disagree with what was delivered.
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "backup",
            "digest",
            "--data-dir",
            data.to_str().unwrap(),
            "--backup-root",
            root.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    let local: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(local["kind"], delivered["kind"]);
    assert_eq!(local["title"], delivered["title"]);
    assert_eq!(local["action"], delivered["action"]);
    let states = |note: &Value| {
        let mut pairs: Vec<(String, String)> = note["databases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|db| {
                (
                    db["name"].as_str().unwrap().to_string(),
                    db["state"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        pairs.sort();
        pairs
    };
    assert_eq!(states(&local), states(&delivered));
    // alpha has no verified copy, so the command is a nonzero condition, not
    // a report: a schedule that reads as success is how a backup stops being
    // checked.
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("backup_unhealthy"));

    // The gateway is up here, so prove the other half of the claim: it works
    // with the gateway stopped, which is when an operator needs it.
    drop(server);
    let stopped = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "backup",
            "digest",
            "--data-dir",
            data.to_str().unwrap(),
            "--backup-root",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    // The human digest names the database, in a per-database table row
    // that carries its state alongside the name.
    let digest = String::from_utf8_lossy(&stopped.stdout);
    assert!(
        digest.contains("│ alpha") && digest.contains("unknown"),
        "human digest must name the database and its state: {digest}"
    );
    assert_eq!(stopped.status.code(), Some(1));
}

/// The collector is a hand-rolled HTTP server; this is the assertion that it
/// still works. Without it, every delivery test below fails for the same
/// uninteresting reason if the harness breaks.
#[test]
fn the_collector_records_a_post() {
    let collector = Collector::start();
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(collector.url())
        .json(&json!({"kind": "probe", "n": 7}))
        .timeout(Duration::from_secs(10))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let note = collector.wait_for("probe", Duration::from_secs(10), |note| note["n"] == 7);
    assert_eq!(note["kind"], "probe");
}

#[test]
fn a_webhook_the_operator_cannot_mean_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let _ = init(&data);
    // A digest carries database names and backup state, so a cleartext
    // non-loopback sink is refused rather than quietly accepted, and refused
    // BEFORE the listener opens -- an operator who sees this exit code has
    // not half-enabled delivery they think they have.
    for url in [
        "http://example.com/hook",
        "not a url",
        "ftp://127.0.0.1/hook",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args([
                "serve",
                "--listen",
                "127.0.0.1:0",
                "--data-dir",
                data.to_str().unwrap(),
                "--backup-digest-secs",
                "60",
                "--backup-notify-webhook",
                url,
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{url} was accepted");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid_backup_webhook"),
            "{url} failed without naming the flag: {stderr}"
        );
        assert!(
            !stderr.contains("liveness server listening"),
            "{url} started a listener anyway: {stderr}"
        );
    }
}
