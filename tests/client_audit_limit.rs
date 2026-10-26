//! Regression: `dg audit` must keep working as the trail fills.
//!
//! The audit log is bounded by the gateway (2000 entries, each a
//! statement shape), so a full trail is far larger than one query
//! result. The CLI used to cap every generic response at the query size
//! (256 KiB), which meant `dg audit` answered fine on a quiet box and
//! failed with "response too large; the gateway response exceeded the
//! configured limit" once the log had been used — the command stopped
//! working precisely because the gateway had done its job.
//!
//! These tests use a fake gateway that answers `/v1/audit` with a body
//! larger than a query result, so the limit is pinned without needing a
//! real trail to fill.
//!
//! Run: `timeout 300 cargo test --locked --test client_audit_limit`

use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};

/// How many entries the fake trail holds.
const ENTRIES: usize = 2000;

/// A fake gateway answering `/v1/audit` with a full-sized trail.
struct FakeGateway {
    address: String,
}

impl FakeGateway {
    fn start(body: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let trimmed = line.trim_end().to_string();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = trimmed.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                        && let Ok(length) = value.trim().parse::<usize>()
                    {
                        content_length = length;
                    }
                }
                if content_length > 0 {
                    let mut discard = vec![0u8; content_length];
                    let _ = std::io::Read::read_exact(&mut reader, &mut discard);
                }
                let text = body.to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        text.len(),
                        text
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            }
        });
        Self { address }
    }
}

fn key_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owner.key");
    std::fs::write(&path, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

/// A trail of `ENTRIES` entries whose shapes are ordinary statements.
fn trail() -> Value {
    let detail = "SELECT id, sku, name, price_cents, stock, category FROM products WHERE category = ? AND stock > ? ORDER BY price_cents DESC LIMIT ?";
    let entries: Vec<Value> = (0..ENTRIES)
        .map(|index| {
            json!({
                "at": 1_793_000_000u64 + index as u64,
                "principal": "owner",
                "role": "owner",
                "action": "query",
                "database": "inventory",
                "detail": detail,
            })
        })
        .collect();
    json!({ "entries": entries })
}

#[test]
fn a_full_audit_trail_is_read_rather_than_refused() {
    // The body below is bigger than the CLI's query-result cap. Before
    // the fix this command exited nonzero with "response too large".
    let body = trail();
    let size = body.to_string().len();
    assert!(
        size > dg::execution::MAX_RESULT,
        "the fixture must be larger than a query result to prove anything: {size} bytes"
    );
    let gateway = FakeGateway::start(body);
    let key = key_file();
    let out = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            &gateway.address,
            "--key-file",
            key.path().join("owner.key").to_str().unwrap(),
            "audit",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "dg audit failed on a full trail ({size} bytes): stderr={stderr}"
    );
    assert!(
        !stderr.contains("response too large"),
        "the CLI still refuses a full trail: {stderr}"
    );
    // The answer is the API's document, unchanged.
    let parsed: Value = serde_json::from_str(&stdout).expect("the answer is JSON");
    assert_eq!(
        parsed["entries"].as_array().map(Vec::len),
        Some(ENTRIES),
        "the CLI dropped entries from the answer"
    );
}

#[test]
fn the_audit_limit_is_the_trail_size_not_the_query_size() {
    // Pinning the intent: the audit read must be allowed to carry what
    // the gateway can hold. A future change that quietly puts the audit
    // path back under the generic cap would make `dg audit` fail again
    // exactly when the trail got full, which is the defect this file
    // exists for.
    let body = trail();
    let size = body.to_string().len();
    assert!(
        size > dg::execution::MAX_RESULT,
        "the fixture must exceed the query cap: {size} bytes"
    );
    let gateway = FakeGateway::start(body);
    let key = key_file();
    // The same read twice: both must succeed, so the limit is not
    // accidentally one-shot.
    for attempt in 0..2 {
        let out = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args([
                "--server",
                &gateway.address,
                "--key-file",
                key.path().join("owner.key").to_str().unwrap(),
                "audit",
                "--limit",
                "5",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "attempt {attempt} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
