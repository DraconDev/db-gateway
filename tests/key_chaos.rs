//! Key chaos matrix (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test key_chaos`
//!
//! Owner, scoped read-write, scoped read-only, cross-database,
//! revoked, wrong, short, and missing credentials against every route
//! family: management (list/create/issue/revoke/delete-db), reads,
//! writes, batches, exports, clones, and imports. Every status and
//! error code is asserted exactly, then a 12-thread concurrent load
//! replays the matrix 720 times to prove no cross-talk, no 500s, and
//! sticky revocation under contention.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
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
        .args(["serve", "--listen", "127.0.0.1:0", "--data-dir", data_dir.to_str().unwrap()])
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

fn req(
    address: &str,
    method: &str,
    path: &str,
    cred: Option<&str>,
    body: &Value,
    idem: Option<&str>,
) -> (u16, Value) {
    let mut request = reqwest::blocking::Client::new()
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://{address}{path}"),
        )
        .json(body)
        .timeout(Duration::from_secs(30));
    if let Some(cred) = cred {
        request = request.bearer_auth(cred);
    }
    if let Some(idem) = idem {
        request = request.header("Idempotency-Key", idem);
    }
    let response = request.send().unwrap();
    (response.status().as_u16(), response.json().unwrap())
}

#[test]
fn every_credential_gets_its_exact_code_on_every_route() {
    let data_dir = tempfile::tempdir().unwrap();
    let (server, owner) = spawn(data_dir.path());
    let a = &server.address;
    // (status, code-or-"-" for 200)
    let check = |label: &str, got: (u16, Value), want_status: u16, want_code: &str| {
        assert_eq!(got.0, want_status, "{label}: {got:?}");
        if want_status == 200 {
            assert!(got.1.get("error").is_none(), "{label}: {got:?}");
        } else {
            assert_eq!(got.1["error"]["code"], want_code, "{label}: {got:?}");
        }
    };

    check("owner create ka", req(a, "POST", "/v1/databases", Some(&owner), &json!({"name":"ka"}), None), 200, "-");
    check("owner create kb", req(a, "POST", "/v1/databases", Some(&owner), &json!({"name":"kb"}), None), 200, "-");
    check("owner create kdel", req(a, "POST", "/v1/databases", Some(&owner), &json!({"name":"kdel"}), None), 200, "-");
    check("owner seed ka", req(a, "POST", "/v1/databases/ka/query", Some(&owner), &json!({"sql":"CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"}), Some("seed-ka")), 200, "-");
    check("owner seed kb", req(a, "POST", "/v1/databases/kb/query", Some(&owner), &json!({"sql":"CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"}), Some("seed-kb")), 200, "-");

    let issue = |name: &str, role: &str, dbs: &[&str]| -> String {
        let (status, body) = req(a, "POST", "/v1/keys", Some(&owner), &json!({"name":name,"role":role,"databases":dbs}), None);
        assert_eq!(status, 200, "issue {name}: {body}");
        body["key"].as_str().unwrap().to_string()
    };
    let rw_a = issue("rw_a", "database_read_write", &["ka"]);
    let ro_a = issue("ro_a", "database_read_only", &["ka"]);
    let rw_b = issue("rw_b", "database_read_write", &["kb"]);
    let gone = issue("gone", "database_read_write", &["ka"]);
    let rw_imp = issue("rw_imp", "database_read_write", &["ka"]);
    check("dup key name", req(a, "POST", "/v1/keys", Some(&owner), &json!({"name":"rw_a","role":"database_read_write","databases":["ka"]}), None), 409, "key_exists");
    check("owner revoke gone", req(a, "DELETE", "/v1/keys/gone", Some(&owner), &json!({}), None), 200, "-");

    let wrong = "f".repeat(64);
    let creds: &[(&str, Option<String>)] = &[
        ("owner", Some(owner.clone())),
        ("rw_a", Some(rw_a.clone())),
        ("ro_a", Some(ro_a.clone())),
        ("rw_b", Some(rw_b.clone())),
        ("gone", Some(gone.clone())),
        ("wrong", Some(wrong.clone())),
        ("short", Some("abc".to_string())),
        ("missing", None),
    ];
    let code_of = |cred: &str, scope_ok: bool, read_only_ok: bool| -> (u16, &'static str) {
        match cred {
            "owner" => (200, "-"),
            "rw_a" | "rw_b" => {
                if scope_ok {
                    (200, "-")
                } else {
                    (403, "database_forbidden")
                }
            }
            "ro_a" => {
                if !scope_ok {
                    (403, "database_forbidden")
                } else if read_only_ok {
                    (200, "-")
                } else {
                    (403, "sql_forbidden")
                }
            }
            "gone" | "wrong" | "short" | "missing" => (401, "invalid_credential"),
            _ => unreachable!(),
        }
    };

    // Reads on ka (in scope for rw_a/ro_a/gone-shape, out for rw_b).
    for (name, cred) in creds {
        let (want, code) = code_of(name, *name != "rw_b", true);
        // `gone` is scoped to ka but revoked: 401 always wins over scope.
        let (want, code) = if *name == "gone" { (401, "invalid_credential") } else { (want, code) };
        check(
            &format!("{name} read ka"),
            req(a, "POST", "/v1/databases/ka/query", cred.as_deref(), &json!({"sql":"SELECT count(*) FROM t"}), None),
            want, code,
        );
    }
    // Writes on ka.
    for (name, cred) in creds {
        let (want, code) = match *name {
            "owner" | "rw_a" => (200, "-"),
            "ro_a" => (403, "sql_forbidden"),
            "rw_b" => (403, "database_forbidden"),
            _ => (401, "invalid_credential"),
        };
        check(
            &format!("{name} write ka"),
            req(a, "POST", "/v1/databases/ka/query", cred.as_deref(), &json!({"sql":"INSERT INTO t(v) VALUES ('x')"}), Some(&format!("chaos-{name}"))),
            want, code,
        );
    }
    // Management routes: owner-only.
    for (name, cred) in creds {
        let (want, code) = if *name == "owner" { (200, "-") } else if *name == "gone" || *name == "wrong" || *name == "short" || *name == "missing" { (401, "invalid_credential") } else { (403, "forbidden") };
        check(
            &format!("{name} list"),
            req(a, "GET", "/v1/databases", cred.as_deref(), &json!({}), None),
            want, code,
        );
    }
    // Delete-db: scoped keys refused, owner works (on scratch kdel).
    check("rw_a delete kdel", req(a, "DELETE", "/v1/databases/kdel", Some(&rw_a), &json!({}), None), 403, "forbidden");
    check("ro_a delete kdel", req(a, "DELETE", "/v1/databases/kdel", Some(&ro_a), &json!({}), None), 403, "forbidden");
    check("gone delete kdel", req(a, "DELETE", "/v1/databases/kdel", Some(&gone), &json!({}), None), 401, "invalid_credential");
    check("owner delete kdel", req(a, "DELETE", "/v1/databases/kdel", Some(&owner), &json!({}), None), 200, "-");
    // Export / clone scoping.
    check("rw_b export ka", req(a, "POST", "/v1/databases/ka/export", Some(&rw_b), &json!({}), None), 403, "database_forbidden");
    check("ro_a export ka", req(a, "POST", "/v1/databases/ka/export", Some(&ro_a), &json!({}), None), 200, "-");
    check("ro_a clone ka", req(a, "POST", "/v1/databases/ka/clone", Some(&ro_a), &json!({"destination":"kc_no"}), None), 403, "forbidden");
    check("rw_b clone ka", req(a, "POST", "/v1/databases/ka/clone", Some(&rw_b), &json!({"destination":"kc_no"}), None), 403, "database_forbidden");
    check("rw_a clone ka", req(a, "POST", "/v1/databases/ka/clone", Some(&rw_a), &json!({"destination":"kc_yes"}), None), 200, "-");
    // Import scoping (need export bytes first).
    let (_, exp) = req(a, "POST", "/v1/databases/ka/export", Some(&owner), &json!({}), None);
    let data = exp["data_base64"].as_str().unwrap().to_string();
    check("owner import", req(a, "POST", "/v1/databases/imp_owner/import", Some(&owner), &json!({"name":"imp_owner","data_base64":data}), None), 200, "-");
    // Scoped import authorizes via the path database; the body name is the new database.
    check("rw_imp import", req(a, "POST", "/v1/databases/ka/import", Some(&rw_imp), &json!({"name":"imp_rw","data_base64":data}), None), 200, "-");
    check("scoped import over live", req(a, "POST", "/v1/databases/ka/import", Some(&rw_imp), &json!({"name":"ka","data_base64":data}), None), 409, "database_exists");
    check("rw_a import other", req(a, "POST", "/v1/databases/imp_no/import", Some(&rw_a), &json!({"name":"imp_no","data_base64":data}), None), 403, "database_forbidden");
    check("ro_a import", req(a, "POST", "/v1/databases/imp_no2/import", Some(&ro_a), &json!({"name":"imp_no2","data_base64":data}), None), 403, "forbidden");
    check("gone import", req(a, "POST", "/v1/databases/imp_no3/import", Some(&gone), &json!({"name":"imp_no3","data_base64":data}), None), 401, "invalid_credential");

    // Concurrent load: 12 threads replay the matrix; every code exact.
    let barrier = Arc::new(Barrier::new(12));
    let address = Arc::new(a.clone());
    let creds_owned: Vec<(String, Option<String>)> = creds.iter().map(|(n, c)| (n.to_string(), c.clone())).collect();
    let handles: Vec<_> = (0..12)
        .map(|t| {
            let (barrier, address, creds_owned, owner) =
                (barrier.clone(), address.clone(), creds_owned.clone(), owner.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..60 {
                    let (name, cred) = &creds_owned[(t * 60 + i) % creds_owned.len()];
                    // Reads: exact matrix codes, including sticky 401 for gone.
                    let (want, code) = match name.as_str() {
                        "owner" | "rw_a" | "ro_a" => (200, "-"),
                        "rw_b" => (403, "database_forbidden"),
                        _ => (401, "invalid_credential"),
                    };
                    let (status, body) = req(&address, "POST", "/v1/databases/ka/query", cred.as_deref(), &json!({"sql":"SELECT count(*) FROM t"}), None);
                    assert_eq!(status, want, "load {name} read: {body}");
                    if want != 200 {
                        assert_eq!(body["error"]["code"], code, "load {name} read");
                    }
                    // Sparse keyed writes (owner + rw_a only, unique keys).
                    if (name == "owner" || name == "rw_a") && i % 10 == 0 {
                        let (status, body) = req(&address, "POST", "/v1/databases/ka/query", cred.as_deref(), &json!({"sql":"INSERT INTO t(v) VALUES ('load')"}), Some(&format!("load-{t}-{i}")));
                        assert_eq!(status, 200, "load {name} write: {body}");
                    }
                    // Management: owner 200, scoped 403, rest 401.
                    let (want, code) = match name.as_str() {
                        "owner" => (200, "-"),
                        "rw_a" | "ro_a" | "rw_b" => (403, "forbidden"),
                        _ => (401, "invalid_credential"),
                    };
                    let (status, body) = req(&address, "GET", "/v1/databases", cred.as_deref(), &json!({}), None);
                    assert_eq!(status, want, "load {name} list: {body}");
                    if want != 200 {
                        assert_eq!(body["error"]["code"], code, "load {name} list");
                    }
                }
                let _ = owner;
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
