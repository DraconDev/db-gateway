//! Regression tests for the CLI's handling of hostile wire data.
//!
//! The CLI draws whatever the gateway says, and the gateway is reached
//! over the network. A value carrying terminal escape sequences must
//! never be able to repaint the terminal, and a value that is not the
//! shape the CLI expects must never panic it.
//!
//! These are tested against a FAKE gateway rather than a real one,
//! because a real gateway validates its own inputs: it will not hand
//! out a database name containing an ESC byte, and it will not hand out
//! a backup stamp in Japanese. A mismatched gateway, an intercepting
//! proxy, or a future version of the server can, and the CLI is the last
//! thing standing between that value and the user's terminal.
//!
//! Run: `timeout 300 cargo test --locked --test cli_hostile_wire`

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};

/// A value that tries to repaint the terminal: colour, then reset, then
/// clear the screen. If any of it reaches the terminal raw, the grid
/// visibly breaks.
const EVIL: &str = "evil\u{1b}[31mRED\u{1b}[0m\u{1b}[2J";

/// A backup-set stamp whose prefix is multi-byte. The CLI used to slice
/// the stamp as a string after checking its shape on the bytes, which
/// panicked on a slice point inside a character.
const EVIL_STAMP: &str = "日本abT123456";

/// A fake gateway that answers every path with the same hostile body.
struct FakeGateway {
    address: String,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeGateway {
    /// Start a gateway that answers with the body for the requested path.
    fn start(routes: Vec<(&'static str, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                // Read the request head: the request line, then the
                // headers, then the body if the request had one.
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
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                }
                // The path is the second word of the request line.
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let body = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_else(|| json!({"error": {"code": "not_found"}}).to_string());
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            }
        });
        Self {
            address,
            thread: Some(thread),
        }
    }

    fn url(&self) -> &str {
        &self.address
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        // The listener is closed when the thread ends; the thread ends
        // when the process ends. Nothing to join that would block.
        let _ = self.thread.take();
    }
}

/// A credential file the CLI will accept: 64 hex characters, readable by
/// its owner only.
fn key_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owner.key");
    std::fs::write(&path, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

/// Run a command under a pty so the CLI takes its terminal path, and
/// return the raw bytes it wrote.
fn pty(server: &str, key: &std::path::Path, args: &[&str]) -> (bool, String) {
    let command = format!(
        "{} --server {} --key-file {} {}",
        env!("CARGO_BIN_EXE_dg"),
        server,
        key.to_str().unwrap(),
        args.join(" ")
    );
    let out = Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Strip ANSI escapes so a text assertion sees the words, not the
/// colour codes around them.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

/// The number of escape bytes in the output. The renderer emits its own
/// colour sequences, so a raw escape byte from a VALUE would add to
/// this count.
fn renderer_escapes(text: &str) -> usize {
    text.chars().filter(|c| *c == '\u{1b}').count()
}

#[test]
fn a_database_name_from_the_wire_cannot_repaint_the_terminal() {
    // `dg list` draws the database names the gateway reports. A name
    // carrying escape sequences must be shown escaped, and must not
    // break the grid around it.
    let gateway = FakeGateway::start(vec![(
        "/v1/databases",
        json!({"databases": [EVIL, "ordinary"]}),
    )]);
    let key = key_file();
    let (ok, screen) = pty(
        gateway.url(),
        key.path().join("owner.key").as_path(),
        &["list"],
    );
    assert!(ok, "dg list failed: {screen:?}");

    // The value is visible, and visible as escaped text.
    assert!(screen.contains("evil"), "the value was dropped: {screen:?}");
    assert!(
        screen.contains("\\u{1b}"),
        "the value was not escaped: {screen:?}"
    );

    // The grid is intact: every row line has the same width, so the
    // escaped value did not shear the table.
    let text = plain(&screen);
    let lines: Vec<&str> = text.lines().filter(|line| line.starts_with('│')).collect();
    assert!(lines.len() >= 2, "no table rows: {screen:?}");
    let widths: Vec<usize> = lines.iter().map(|line| line.chars().count()).collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "the grid sheared: {lines:?}"
    );
}

#[test]
fn no_dashboard_view_lets_a_wire_value_repaint_the_terminal() {
    // Every human-facing view draws wire data into cells: a database
    // name, a key name, a service version, a backup state. Each one is
    // checked here, because the escaping rule has to hold for all of
    // them, not just for the one that happened to be reported.
    //
    // The check is differential: the same view is drawn twice, once
    // with a hostile value and once with a benign one, and the number
    // of escape bytes the terminal receives must be the same. A value
    // that reached the terminal raw would add escape bytes of its own.
    let hostile_routes = vec![
        ("/v1/databases", json!({"databases": [EVIL]})),
        (
            "/v1/databases/demo",
            json!({
                "name": EVIL,
                "tombstoned": false,
                "size_bytes": 4096,
                "created_at": 1_700_000_000,
                "tables": [{"name": EVIL, "rows": 3}],
            }),
        ),
        (
            "/v1/status",
            json!({
                "version": EVIL,
                "uptime_secs": 12,
                "admission": {
                    "acquire_timeout_ms": 250,
                    "read_permits": 64,
                    "write_permits": 16,
                },
                "requests": {
                    "total": 5,
                    "capacity_exceeded": 0,
                    "query_deadline_exceeded": 0,
                    "internal_error": 0,
                },
                "databases": {
                    EVIL: {
                        "rows_read": 1,
                        "rows_written": 2,
                        "queue_waits": 0,
                        "queue_wait_ms": 0,
                        "queue_timeouts": 0,
                    }
                },
            }),
        ),
        (
            "/v1/keys",
            json!({"keys": [{"name": EVIL, "role": "owner", "databases": []}]}),
        ),
        (
            "/v1/backups",
            json!({
                "backup_root": "/tmp/backups",
                "stale_after_secs": 300,
                "sets": [],
                "per_database": {
                    EVIL: {"state": "fresh", "newest_set": "20261019T231511Z", "age_secs": 4}
                },
            }),
        ),
    ];
    let benign_routes = vec![
        ("/v1/databases", json!({"databases": ["ordinary"]})),
        (
            "/v1/databases/demo",
            json!({
                "name": "ordinary",
                "tombstoned": false,
                "size_bytes": 4096,
                "created_at": 1_700_000_000,
                "tables": [{"name": "ordinary", "rows": 3}],
            }),
        ),
        (
            "/v1/status",
            json!({
                "version": "0.2.0",
                "uptime_secs": 12,
                "admission": {
                    "acquire_timeout_ms": 250,
                    "read_permits": 64,
                    "write_permits": 16,
                },
                "requests": {
                    "total": 5,
                    "capacity_exceeded": 0,
                    "query_deadline_exceeded": 0,
                    "internal_error": 0,
                },
                "databases": {
                    "ordinary": {
                        "rows_read": 1,
                        "rows_written": 2,
                        "queue_waits": 0,
                        "queue_wait_ms": 0,
                        "queue_timeouts": 0,
                    }
                },
            }),
        ),
        (
            "/v1/keys",
            json!({"keys": [{"name": "ordinary", "role": "owner", "databases": []}]}),
        ),
        (
            "/v1/backups",
            json!({
                "backup_root": "/tmp/backups",
                "stale_after_secs": 300,
                "sets": [],
                "per_database": {
                    "ordinary": {"state": "fresh", "newest_set": "20261019T231511Z", "age_secs": 4}
                },
            }),
        ),
    ];

    let hostile = FakeGateway::start(hostile_routes);
    let benign = FakeGateway::start(benign_routes);
    let key = key_file();
    let key_path = key.path().join("owner.key");
    let key_path = key_path.as_path();

    for args in [
        vec!["list"],
        vec!["info", "demo"],
        vec!["status"],
        vec!["key", "list"],
        vec!["backup", "status"],
    ] {
        let (ok, screen) = pty(hostile.url(), key_path, &args);
        assert!(ok, "dg {} failed: {screen:?}", args.join(" "));
        let (ok, clean) = pty(benign.url(), key_path, &args);
        assert!(ok, "dg {} failed: {clean:?}", args.join(" "));

        // The hostile value is visible, and visible as escaped text.
        assert!(
            screen.contains("evil"),
            "dg {} dropped the value: {screen:?}",
            args.join(" ")
        );
        assert!(
            screen.contains("\\u{1b}"),
            "dg {} did not escape the value: {screen:?}",
            args.join(" ")
        );

        // The value contributed no escape bytes of its own: the hostile
        // view sends the terminal exactly as many as the benign one.
        assert_eq!(
            renderer_escapes(&screen),
            renderer_escapes(&clean),
            "dg {} let the value repaint the terminal: {screen:?}",
            args.join(" ")
        );
    }
}

#[test]
fn a_malformed_backup_stamp_does_not_panic_the_cli() {
    // The backup view humanises a set stamp. A stamp whose prefix is
    // multi-byte used to panic the CLI: the shape was checked on the
    // bytes, but the string was sliced anyway, and the slice landed
    // inside a character.
    let gateway = FakeGateway::start(vec![(
        "/v1/backups",
        json!({
            "backup_root": "/tmp/backups",
            "stale_after_secs": 300,
            "sets": [],
            "per_database": {
                "demo": {"state": "fresh", "newest_set": EVIL_STAMP, "age_secs": 4}
            },
        }),
    )]);
    let key = key_file();
    let key_path = key.path().join("owner.key");
    let (ok, screen) = pty(gateway.url(), key_path.as_path(), &["backup", "status"]);
    assert!(ok, "dg backup status failed: {screen:?}");
    assert!(
        !screen.contains("panicked"),
        "the CLI panicked on a malformed stamp: {screen:?}"
    );
    // The value is shown as it arrived, not rewritten into a stamp.
    assert!(
        screen.contains(EVIL_STAMP),
        "the malformed stamp was dropped: {screen:?}"
    );
}
