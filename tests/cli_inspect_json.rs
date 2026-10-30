//! `dg inspect --json`: the same facts a terminal shows, off a terminal.
//!
//! Run: `timeout 300 cargo test --locked --test cli_inspect_json`
//!
//! The contract these tests pin: `dg inspect` answers "what has this
//! database been doing" — activity and backup state — and it answers
//! that the same way whether a person is watching or a script is
//! reading. Before this, piped output silently degraded to the bare
//! info document, so a script got a different, poorer answer than the
//! operator looking at the same command on the same box.
//!
//! The second half pins the degradation rule: activity and backup are
//! auxiliary reads. When one of them cannot be fetched the section is
//! reported absent, not invented as zero — a zero would read as "no
//! traffic" rather than "we could not ask".

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// Create `shop` with a couple of tables so `tables` has something to
/// report, and write a row so activity has a fact to carry.
fn seed(fleet: &Fleet) {
    fleet.cli.ok("create", &["create", "shop"]);
    fleet.cli.q(
        "create products",
        "shop",
        "CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT)",
    );
    fleet.cli.q(
        "insert",
        "shop",
        "INSERT INTO products (id, sku) VALUES (1, 'A'), (2, 'B')",
    );
}

/// `dg inspect --json` for one database.
fn inspect(fleet: &Fleet, label: &str, db: &str) -> Value {
    fleet.cli.ok(label, &["inspect", "--json", db])
}

#[test]
fn a_pipe_gets_the_activity_a_terminal_shows() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = inspect(&fleet, "inspect", "shop");

    // The facts a person sees. A script that cannot read this cannot
    // answer "what is this database doing" without rejoining three
    // endpoints by hand.
    assert!(
        document["activity"].is_object(),
        "activity section is missing: {document}"
    );
    assert_eq!(
        document["activity"]["rows_written"].as_u64(),
        Some(2),
        "the writes just made are not in the activity section: {document}"
    );
    assert!(
        document["activity"]["rows_read"].is_u64(),
        "activity carries no read counter: {document}"
    );
    // Availability is stated rather than implied: a script must be able
    // to tell "no activity" from "could not ask".
    assert_eq!(
        document["activity_available"].as_bool(),
        Some(true),
        "activity_available should be true on a healthy gateway: {document}"
    );
}

#[test]
fn a_pipe_gets_the_backup_state_a_terminal_shows() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = inspect(&fleet, "inspect", "shop");

    assert!(
        document["backup"].is_object(),
        "backup section is missing: {document}"
    );
    // The state vocabulary is the same one `dg backup status` uses, so
    // a script greps one word and not two.
    let state = document["backup"]["state"]
        .as_str()
        .expect("backup carries a state");
    assert!(
        matches!(state, "fresh" | "behind" | "stale" | "unknown" | "failed"),
        "unexpected backup state {state:?}: {document}"
    );
    assert_eq!(
        document["backup_available"].as_bool(),
        Some(true),
        "backup_available should be true on a healthy gateway: {document}"
    );
}

#[test]
fn the_info_the_database_always_had_is_still_there() {
    let fleet = Fleet::boot();
    seed(&fleet);
    let document = inspect(&fleet, "inspect", "shop");

    // Composition added sections; it did not replace the document a
    // caller already depended on.
    assert_eq!(document["name"].as_str(), Some("shop"));
    assert!(document["size_bytes"].is_u64(), "{document}");
    assert_eq!(document["tombstoned"].as_bool(), Some(false));
    let tables: Vec<&str> = document["tables"]
        .as_array()
        .expect("tables list")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tables.contains(&"products"),
        "the table list lost a table: {tables:?}"
    );
}

#[test]
fn a_tombstoned_database_still_answers() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet
        .cli
        .ok("delete", &["delete", "shop", "--confirm", "shop"]);
    // A deleted database is exactly when an operator most wants to ask
    // what it was doing, so the command must not refuse.
    let document = inspect(&fleet, "inspect", "shop");
    assert_eq!(
        document["tombstoned"].as_bool(),
        Some(true),
        "a tombstone should be visible: {document}"
    );
    assert!(
        document["purge_after"].is_u64(),
        "a tombstone reports when it will be purged: {document}"
    );
}

#[test]
fn the_auxiliary_sections_degrade_instead_of_failing() {
    for failed_path in ["/v1/status", "/v1/backups"] {
        let document = inspect_with_auxiliary_failure(failed_path);
        // The required database fact still succeeds, and both auxiliary
        // keys retain their schema; only the failing section says it was
        // unavailable. This is the TTY path's degradation behavior carried
        // into the machine-readable answer.
        assert_eq!(document["name"].as_str(), Some("shop"), "{document}");
        assert!(document["activity"].is_object(), "{document}");
        assert!(document["backup"].is_object(), "{document}");
        match failed_path {
            "/v1/status" => {
                assert_eq!(document["activity_available"].as_bool(), Some(false));
                assert_eq!(document["backup_available"].as_bool(), Some(true));
                assert_eq!(document["backup"]["state"].as_str(), Some("fresh"));
            }
            "/v1/backups" => {
                assert_eq!(document["activity_available"].as_bool(), Some(true));
                assert_eq!(document["backup_available"].as_bool(), Some(false));
                assert_eq!(document["activity"]["rows_read"].as_u64(), Some(9));
            }
            _ => unreachable!(),
        }
    }
}

/// Run `dg inspect --json` against a tiny HTTP fixture that fails exactly
/// one auxiliary endpoint. The database read remains healthy, so the
/// command's ability to degrade rather than fail is directly exercised.
fn inspect_with_auxiliary_failure(failed_path: &str) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let failed_path = failed_path.to_string();
    let server = thread::spawn(move || {
        let mut observed = Vec::new();
        for _ in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("stub did not receive three reads: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap().to_string();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            observed.push(path.clone());
            let (status, body) = if path == failed_path {
                (
                    "503 Service Unavailable",
                    r#"{"error":{"code":"unavailable","message":"test fixture"}}"#,
                )
            } else if path == "/v1/databases/shop" {
                (
                    "200 OK",
                    r#"{"name":"shop","created_at":1,"size_bytes":10,"tombstoned":false,"purge_after":null,"tables":[]}"#,
                )
            } else if path == "/v1/status" {
                (
                    "200 OK",
                    r#"{"databases":{"shop":{"rows_read":9,"rows_written":2,"queue_waits":0}}}"#,
                )
            } else {
                (
                    "200 OK",
                    r#"{"per_database":{"shop":{"state":"fresh","newest_set":"20261030T000000Z","age_secs":1}}}"#,
                )
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
        }
        observed
    });

    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("owner.key");
    std::fs::write(&key_path, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--json",
            "--server",
            &format!("http://{address}"),
            "--key-file",
            key_path.to_str().unwrap(),
            "inspect",
            "shop",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "inspect failed on an auxiliary error: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let observed = server.join().unwrap();
    assert_eq!(
        observed,
        ["/v1/databases/shop", "/v1/status", "/v1/backups"]
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
