//! CLI export lifecycle cleanup when retained-snapshot metadata is malformed.

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn read_request(stream: &TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse::<usize>().map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?;
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    request.push_str(&String::from_utf8_lossy(&body));
    Ok(request)
}

fn write_response(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()
}

fn accept_before_deadline(listener: &TcpListener) -> std::io::Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn assert_malformed_begin_releases(begin: Value, snapshot: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let begin_body = begin.to_string();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut request_lines = Vec::new();
            for body in [begin_body, "{}".to_string()] {
                let mut stream = accept_before_deadline(&listener)?;
                let request = read_request(&stream)?;
                request_lines.push(
                    request
                        .lines()
                        .next()
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "missing request line",
                            )
                        })?
                        .to_string(),
                );
                write_response(&mut stream, &body)?;
            }
            Ok::<_, std::io::Error>(request_lines)
        })();
        let _ = sender.send(result);
    });

    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("owner.key");
    std::fs::write(&key_file, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = dir.path().join("export.sqlite");
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            &format!("http://{address}"),
            "--key-file",
            key_file.to_str().unwrap(),
            "export",
            "app",
            "--out",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "malformed export begin unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!out.exists(), "failed export published an output file");

    let request_lines = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("CLI did not close its export lifecycle conversation")
        .unwrap();
    assert_eq!(
        request_lines,
        vec![
            "POST /v1/databases/app/export/begin HTTP/1.1".to_string(),
            format!("DELETE /v1/snapshots/{snapshot} HTTP/1.1"),
        ]
    );
}

#[test]
fn malformed_export_begin_metadata_releases_retained_snapshot() {
    let oversized_snapshot = "a".repeat(64);
    assert_malformed_begin_releases(
        json!({
            "snapshot": oversized_snapshot,
            "size": dg::store::MAX_TRANSFER_SIZE + 1,
            "sha256": "0".repeat(64)
        }),
        &oversized_snapshot,
    );

    let malformed_hash_snapshot = "b".repeat(64);
    assert_malformed_begin_releases(
        json!({
            "snapshot": malformed_hash_snapshot,
            "size": 1,
            "sha256": "not-a-hash"
        }),
        &malformed_hash_snapshot,
    );
}
