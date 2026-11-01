//! Probe: is the documented 64 KiB body cap actually applied on every route?
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn post(port: u16, key: &str, path: &str, body: &[u8]) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    s.flush().unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut out = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => break,
        }
    }
    let status = out.split_whitespace().nth(1).unwrap_or("?").to_string();
    (status.parse().unwrap_or(0), out)
}

#[test]
fn the_documented_body_cap_is_applied_on_every_route() {
    let dir = std::env::temp_dir().join(format!("dg-bl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keyfile: PathBuf = dir.join("owner.key");
    let init = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    let key = String::from_utf8(init.stdout.clone()).unwrap().trim().to_string();
    std::fs::write(&keyfile, init.stdout).unwrap();

    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["serve", "--data-dir"])
        .arg(&dir)
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", free_port()))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut addr = String::new();
    let mut buf = [0u8; 256];
    loop {
        let n = child.stdout.as_mut().unwrap().read(&mut buf).unwrap();
        if n == 0 { break; }
        addr.push_str(&String::from_utf8_lossy(&buf[..n]));
        if addr.contains("127.0.0.1:") {
            if let Some(i) = addr.find("127.0.0.1:") {
                let rest = &addr[i..];
                let end = rest.find(|c: char| !c.is_ascii_digit() && c != ':').unwrap_or(rest.len());
                let port = rest[..end].rsplit(':').next().unwrap();
                addr = port.to_string();
                break;
            }
        }
    }
    let port: u16 = addr.parse().unwrap();
    println!("port {port}");

    // 100 KiB body: over the documented 64 KiB cap, under axum's 2 MiB default.
    let big = format!("{{\"left\":\"a\",\"right\":\"b\",\"pad\":\"{}\"}}", "x".repeat(100_000));
    for path in ["/v1/diff", "/v1/branch", "/v1/databases/x/vacuum"] {
        let (status, body) = post(port, &key, path, big.as_bytes());
        let first = body.lines().nth(1).unwrap_or("").chars().take(90).collect::<String>();
        println!("{path} -> {status} | {first}");
    }
    // control: a route that is NOT merged (the main router's own query route)
    let (status, body) = post(port, &key, "/v1/databases/x/query", big.as_bytes());
    let first = body.lines().nth(1).unwrap_or("").chars().take(90).collect::<String>();
    println!("/v1/databases/x/query (control) -> {status} | {first}");

    child.kill().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
