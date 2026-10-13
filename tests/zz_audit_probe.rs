//! TEMPORARY AUDIT PROBE — not part of the suite. Deleted after the run.
//!
//! Reports the status+code an OWNER gets for a malformed path name on each
//! `/v1/databases/{name}/...` handler, so the audit can compare them against
//! the 400 invalid_name contract the CLI enforces via validate_name.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};

struct Server {
    child: Child,
    address: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(data_dir: &std::path::Path) -> (Server, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "init failed");
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut address = String::new();
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if let Some(rest) = line.split("listening on ").nth(1) {
            address = rest.trim().to_string();
            break;
        }
    }
    assert!(!address.is_empty(), "no listening line");
    std::thread::sleep(Duration::from_millis(200));
    (Server { child, address }, owner)
}

fn hit(
    client: &reqwest::blocking::Client,
    address: &str,
    owner: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, String) {
    let mut req = client
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://{address}{path}"),
        )
        .bearer_auth(owner)
        .timeout(Duration::from_secs(10));
    if let Some(b) = body {
        req = req.json(b);
    }
    match req.send() {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().unwrap_or_default();
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("code").cloned())
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "<no-code>".to_string());
            (status, code)
        }
        Err(e) => (0, format!("<transport: {e}>")),
    }
}

#[test]
fn owner_malformed_path_name_report() {
    let dir = tempfile::tempdir().unwrap();
    let (server, owner) = spawn(dir.path());
    let client = reqwest::blocking::Client::new();
    let a = &server.address;

    // Create one well-formed database so lookups have something to miss.
    let (status, _) = hit(&client, a, &owner, "POST", "/v1/databases", Some(&json!({"name": "good"})));
    assert_eq!(status, 200, "seed create");

    let bad = "!@#$";
    let mut report: Vec<(String, u16, String)> = Vec::new();

    let q = json!({"sql": "SELECT 1"});
    let b = json!({"statements": []});
    let i = json!({"size": 0, "sha256": "x"});
    let k = json!({"name": "k"});
    let cases: Vec<(&str, &str, Option<&Value>)> = vec![
        ("query", "POST", Some(&q)),
        ("batch", "POST", Some(&b)),
        ("export", "GET", None),
        ("export/begin", "POST", None),
        ("import/begin", "POST", Some(&i)),
        ("", "DELETE", None),
        ("keys", "GET", None),
        ("keys", "POST", Some(&k)),
    ];
    for (path, method, body) in &cases {
        let full = if path.is_empty() {
            format!("/v1/databases/{bad}")
        } else {
            format!("/v1/databases/{bad}/{path}")
        };
        let (status, code) = hit(&client, a, &owner, method, &full, *body);
        report.push((format!("{method} /v1/databases/{{name}}/{path}"), status, code));
    }

    // A well-formed but nonexistent name, for comparison.
    for (path, method, body) in cases.iter() {
        let full = if path.is_empty() {
            "/v1/databases/nope".to_string()
        } else {
            format!("/v1/databases/nope/{path}")
        };
        let (status, code) = hit(&client, a, &owner, method, &full, body.as_ref().map(|v| &**v));
        report.push((format!("{method} /v1/databases/{{nope}}/{path}"), status, code));
    }

    println!("\n===== OWNER PATH-NAME PROBE =====");
    for (label, status, code) in &report {
        println!("{label:45} -> {status} {code}");
    }
    println!("===== END PROBE =====\n");
    eprintln!("PROBE: {} report lines", report.len());
}
