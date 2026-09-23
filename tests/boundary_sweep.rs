//! Boundary sweep at scale (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test boundary_sweep`
//!
//! Every documented wall hit from both sides across ten databases,
//! each accept/reject code asserted exactly:
//! batch 32/33, args 256/257, JSON body 65536/65537 bytes,
//! result rows 1000/1001, result bytes ~240KiB/~280KiB,
//! export ~600KiB/~8.4MiB against the 8MiB wall,
//! import ~600KiB/~8.4MiB against the 8MiB decoded-data wall.
//! A final control database proves rejected requests left no trace.

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
    assert!(output.status.success());
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
    let mut address = None;
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if let Ok(log) = serde_json::from_str::<Value>(&line)
            && let Some(addr) = log["fields"]["address"].as_str()
        {
            address = Some(addr.to_string());
            break;
        }
    }
    (
        Server {
            child,
            address: address.expect("server logged its listen address"),
        },
        owner,
    )
}

fn post(address: &str, cred: &str, path: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://{address}{path}"))
        .bearer_auth(cred)
        .json(body)
        .timeout(Duration::from_secs(120))
        .send()
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().unwrap_or(Value::Null);
    (status, body)
}

fn check(label: &str, got: (u16, Value), want: u16, code: &str) {
    assert_eq!(got.0, want, "{label}: {:?}", got.1);
    if want == 200 {
        assert!(got.1.get("error").is_none(), "{label}: {:?}", got.1);
    } else {
        assert_eq!(got.1["error"]["code"], code, "{label}: {:?}", got.1);
    }
}

/// Build a standalone SQLite file with `rows` 6000-char text rows.
fn big_file(path: &std::path::Path, rows: usize) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t(v TEXT)", []).unwrap();
    let chunk = "x".repeat(6000);
    for _ in 0..rows {
        conn.execute("INSERT INTO t(v) VALUES (?)", [&chunk])
            .unwrap();
    }
    conn.close().unwrap();
}

#[test]
fn every_wall_accepts_at_the_line_and_rejects_past_it() {
    let data_dir = tempfile::tempdir().unwrap();
    let (_server, owner) = spawn(data_dir.path());
    let a = &_server.address;

    for i in 0..10 {
        let db = format!("b{i:02}");
        let (status, _) = post(a, &owner, "/v1/databases", &json!({"name": db}));
        assert_eq!(status, 200, "create {db}");
    }
    let q = |db: &str, body: &Value| post(a, &owner, &format!("/v1/databases/{db}/query"), body);
    let b = |db: &str, body: &Value| post(a, &owner, &format!("/v1/databases/{db}/batch"), body);

    // Batch statement count: 32 admitted, 33 rejected.
    let stmts: Vec<Value> = (0..32)
        .map(|i| json!({"sql": format!("SELECT {i}")}))
        .collect();
    check(
        "batch 32",
        b("b00", &json!({"statements": stmts, "atomic": true})),
        200,
        "-",
    );
    let stmts: Vec<Value> = (0..33)
        .map(|i| json!({"sql": format!("SELECT {i}")}))
        .collect();
    check(
        "batch 33",
        b("b00", &json!({"statements": stmts, "atomic": true})),
        400,
        "invalid_request",
    );

    // Argument count: 256 admitted, 257 rejected.
    let sum256 = (0..256).map(|_| "?").collect::<Vec<_>>().join("+");
    let args256: Vec<Value> = (0..256).map(|_| json!(1)).collect();
    check(
        "args 256",
        q(
            "b01",
            &json!({"sql": format!("SELECT {sum256}"), "args": args256}),
        ),
        200,
        "-",
    );
    let sum257 = (0..257).map(|_| "?").collect::<Vec<_>>().join("+");
    let args257: Vec<Value> = (0..257).map(|_| json!(1)).collect();
    check(
        "args 257",
        q(
            "b01",
            &json!({"sql": format!("SELECT {sum257}"), "args": args257}),
        ),
        400,
        "invalid_request",
    );

    // JSON body: exactly 65536 bytes admitted, 65537 rejected.
    let base = json!({"sql": "SELECT 1"});
    let base_len = serde_json::to_string(&base).unwrap().len();
    for (label, total) in [("body 65536", 65536), ("body 65537", 65537)] {
        let sql = format!("SELECT 1{}", " ".repeat(total - base_len));
        let body = json!({"sql": sql});
        assert_eq!(
            serde_json::to_string(&body).unwrap().len(),
            total,
            "{label} sizing"
        );
        if total == 65536 {
            check(label, q("b02", &body), 200, "-");
        } else {
            check(label, q("b02", &body), 413, "invalid_request");
        }
    }

    // Result rows: 1000 admitted, 1001 rejected.
    let rows = |n: u32| {
        format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{n}) SELECT x FROM c"
        )
    };
    let (status, body) = q("b03", &json!({"sql": rows(1000)}));
    check("rows 1000", (status, body.clone()), 200, "-");
    assert_eq!(body["rows"].as_array().unwrap().len(), 1000);
    check(
        "rows 1001",
        q("b03", &json!({"sql": rows(1001)})),
        413,
        "result_limit_exceeded",
    );

    // Result bytes: ~240KiB admitted, ~280KiB rejected (wall 256KiB).
    let (status, body) = q("b04", &json!({"sql": "SELECT hex(randomblob(120000))"}));
    check("bytes 240KiB", (status, body.clone()), 200, "-");
    assert!(body["rows"][0][0].as_str().unwrap().len() > 200_000);
    check(
        "bytes 280KiB",
        q("b04", &json!({"sql": "SELECT hex(randomblob(140000))"})),
        413,
        "result_limit_exceeded",
    );
    // Two individually bounded statements share one response budget. Without
    // cumulative accounting both execute and the batch is rejected only after
    // retaining roughly twice MAX_RESULT.
    check(
        "aggregate batch bytes",
        b(
            "b04",
            &json!({"statements": [
                {"sql": "SELECT hex(randomblob(120000))"},
                {"sql": "SELECT hex(randomblob(120000))"}
            ], "atomic": true}),
        ),
        413,
        "result_limit_exceeded",
    );

    // Export wall (8MiB): ~600KiB file exports, ~8.4MiB file rejected.
    for (db, rows) in [("b05", 100usize), ("b06", 1400usize)] {
        check(
            &format!("{db} ddl"),
            q(db, &json!({"sql": "CREATE TABLE t(v TEXT)"})),
            200,
            "-",
        );
        for chunk in 0..rows.div_ceil(20) {
            let n = 20.min(rows - chunk * 20);
            let stmts: Vec<Value> = (0..n)
                .map(|_| json!({"sql": "INSERT INTO t(v) SELECT hex(randomblob(3000))"}))
                .collect();
            check(
                &format!("{db} fill {chunk}"),
                b(db, &json!({"statements": stmts, "atomic": true})),
                200,
                "-",
            );
        }
        let (_, cnt) = q(db, &json!({"sql": "SELECT count(*) FROM t"}));
        assert_eq!(cnt["rows"], json!([[rows]]), "{db} fill count");
    }
    let (status, small) = post(a, &owner, "/v1/databases/b05/export", &json!({}));
    check("export 600KiB", (status, small.clone()), 200, "-");
    assert!(small["size"].as_u64().unwrap() > 500_000, "{small:?}");
    check(
        "export 8.4MiB",
        post(a, &owner, "/v1/databases/b06/export", &json!({})),
        413,
        "result_limit_exceeded",
    );

    // Import wall (12MiB body): ~600KiB file imports, ~13MiB file rejected.
    let dir = tempfile::tempdir().unwrap();
    big_file(&dir.path().join("small.db"), 100);
    // Keep the encoded request below the import route's 12MiB transport
    // envelope so the server can consume it and return the decoded 8MiB
    // payload error deterministically instead of closing the upload early.
    big_file(&dir.path().join("big.db"), 1400);
    let small_bytes = std::fs::read(dir.path().join("small.db")).unwrap();
    let big_bytes = std::fs::read(dir.path().join("big.db")).unwrap();
    assert!(
        big_bytes.len() > 8 * 1024 * 1024,
        "big fixture too small for the decoded import wall: {}",
        big_bytes.len()
    );
    use base64::{Engine, engine::general_purpose::STANDARD};
    check(
        "import 600KiB",
        post(
            a,
            &owner,
            "/v1/databases/imp_small/import",
            &json!({"name": "imp_small", "data_base64": STANDARD.encode(&small_bytes)}),
        ),
        200,
        "-",
    );
    check(
        "import 8.4MiB",
        post(
            a,
            &owner,
            "/v1/databases/imp_big/import",
            &json!({"name": "imp_big", "data_base64": STANDARD.encode(&big_bytes)}),
        ),
        413,
        "result_limit_exceeded",
    );

    // Control: rejects left no trace — failed batch wrote nothing, failed
    // import published nothing, b09 untouched and fully functional.
    check(
        "b09 ddl",
        q(
            "b09",
            &json!({"sql": "CREATE TABLE t(id INTEGER PRIMARY KEY)"}),
        ),
        200,
        "-",
    );
    let stmts: Vec<Value> = (0..33)
        .map(|_| json!({"sql": "INSERT INTO t(id) VALUES (1)"}))
        .collect();
    check(
        "b09 bad batch",
        b("b09", &json!({"statements": stmts, "atomic": true})),
        400,
        "invalid_request",
    );
    let (_, cnt) = q("b09", &json!({"sql": "SELECT count(*) FROM t"}));
    assert_eq!(cnt["rows"], json!([[0]]), "rejected batch leaked rows");
    let (status, list) = post(a, &owner, "/v1/databases", &json!({}));
    let _ = (status, list);
    let listed: Vec<String> = reqwest::blocking::Client::new()
        .get(format!("http://{a}/v1/databases"))
        .bearer_auth(&owner)
        .timeout(Duration::from_secs(30))
        .send()
        .unwrap()
        .json::<Value>()
        .unwrap()["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    assert!(
        !listed.contains(&"imp_big".to_string()),
        "rejected import published: {listed:?}"
    );
    for i in 0..10 {
        assert!(
            listed.contains(&format!("b{i:02}")),
            "missing b{i:02}: {listed:?}"
        );
    }
}
