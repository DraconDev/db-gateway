//! `dg rows` is a read, and the transport has to be told so.
//!
//! The CLI classifies a request by its semantics, not by its HTTP method:
//! a query is a POST either way, so the SQL decides. That is what makes
//! a read give up after ten seconds and report an unavailable gateway,
//! while a write gives up after sixty and reports `outcome_unknown` --
//! "writes may have committed, replay only with the same idempotency
//! key".
//!
//! The human path of `dg rows` had two calls that did not declare their
//! semantics, so the client fell back to the method and classified a
//! SELECT as a write: it waited the write timeout, and on failure it
//! told the operator their read might have committed -- advice that is
//! unfollowable, because the command never sends an idempotency key and
//! a SELECT commits nothing. The machine path of the same command
//! already declared `Read`, so the two forms of one command disagreed
//! about what a read is.
//!
//! The test drives the real binary on a real terminal (the human path is
//! only taken when stdout is a terminal) against a gateway that answers
//! the schema request and then DROPS the connection on the row query, so
//! the transport failure is the observable.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The schema document the fake gateway answers with: one table, so the
/// command proceeds to the row query.
const SCHEMA: &str = r#"{"database":"demo","tombstoned":false,"created_at":1700000000,
"tables":[{"name":"notes","rows":3,"columns":[
{"name":"id","type":"INTEGER","not_null":false,"primary_key":true,"default":null},
{"name":"body","type":"TEXT","not_null":false,"primary_key":false,"default":null}],
"indexes":[],"foreign_keys":[],"sql":"CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"}]}"#;

/// A gateway that answers the schema and drops every other connection.
///
/// It listens on an ephemeral port and, for each connection, reads the
/// request head: a request for the schema is answered, and anything else
/// gets the connection closed with no reply at all. That is a transport
/// failure, which is exactly what the client has to classify.
fn fake_gateway() -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if !flag.load(Ordering::Relaxed) {
                return;
            }
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut head = [0u8; 8192];
                let read = match stream.read(&mut head) {
                    Ok(read) => read,
                    Err(_) => return,
                };
                let request = String::from_utf8_lossy(&head[..read]).to_string();
                if request.contains("/schema") {
                    let body = SCHEMA.replace('\n', "");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                // Anything else: drop the connection without a reply.
            });
        }
    });
    (format!("http://{address}"), running)
}

/// Draw `dg rows` on a real terminal and read what an operator reads.
fn rows_on_a_terminal(server: &str) -> String {
    let key_file = std::env::temp_dir().join(format!("dg-rows-tty-{}", std::process::id()));
    // The credential has to LOOK like a credential: the CLI refuses
    // anything that is not 64 hex characters before it opens a
    // connection, and a refused key would hide the transport failure
    // this test is about.
    std::fs::write(&key_file, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();

    // `script` gives the child a terminal on stdout, so the command
    // takes its human path.
    let command = format!(
        "{} --server {} --key-file {} rows demo notes",
        env!("CARGO_BIN_EXE_dg"),
        server,
        key_file.to_str().unwrap()
    );
    let output = Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&key_file);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A row read that cannot reach the gateway is an unavailable gateway.
///
/// It must NOT be reported as `outcome_unknown`: nothing was written, so
/// there is no outcome in doubt, and the retry advice that comes with it
/// ("replay only with the same idempotency key within its retention
/// window") points at a key this command never sends.
#[test]
fn a_row_read_against_a_broken_gateway_is_not_reported_as_a_write() {
    let (server, running) = fake_gateway();
    let text = rows_on_a_terminal(&server);
    running.store(false, Ordering::Relaxed);

    assert!(
        !text.contains("outcome_unknown"),
        "a row read is reported as a write that may have committed: {text}"
    );
    assert!(
        !text.contains("may have committed"),
        "a row read is reported as a write that may have committed: {text}"
    );
    assert!(
        text.contains("transport_unavailable"),
        "a row read against a broken gateway is not an unavailable gateway: {text}"
    );
}
