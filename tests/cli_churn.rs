//! CLI-only churn (dummy data).
//!
//! Run: `timeout 300 cargo test --locked --test cli_churn`
//!
//! Full lifecycle driven through the `dg` binary only: init, create,
//! query (plus keyed replay), batch from file, export to file, import
//! from file, clone, scoped key issue/use/revoke, delete confirmation
//! gate, piped shell session with :batch, and exact error outputs.
//! Zero direct HTTP: this file spawns processes and asserts on
//! argv/stdout/stderr/exit codes only — it must never gain a network
//! client of any kind.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

use serde_json::Value;

struct Cli {
    server: String,
    key_file: std::path::PathBuf,
}

impl Cli {
    /// Run a `dg` subcommand; returns (exit_success, stdout, stderr).
    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let mut full = vec![
            "--server",
            &self.server,
            "--key-file",
            self.key_file.to_str().unwrap(),
        ];
        full.extend(args);
        let output = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args(&full)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn ok(&self, label: &str, args: &[&str]) -> Value {
        let (success, stdout, stderr) = self.run(args);
        assert!(success, "{label} failed: stdout={stdout} stderr={stderr}");
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("{label} not JSON: {stdout}"))
    }

    fn err(&self, label: &str, args: &[&str], code: &str) -> (bool, String, String) {
        let (success, stdout, stderr) = self.run(args);
        assert!(!success, "{label} unexpectedly succeeded: {stdout}");
        assert!(
            stdout.contains(&format!("\"code\":\"{code}\""))
                || stderr.contains(&format!("\"code\":\"{code}\"")),
            "{label} missing code {code}: stdout={stdout} stderr={stderr}"
        );
        (success, stdout, stderr)
    }
}

#[test]
fn full_lifecycle_through_the_binary_only() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");

    // init prints the owner key once; store it 0600 like the docs demand.
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let owner: String = serde_json::from_slice::<Value>(&output.stdout).unwrap()["owner_key"]
        .as_str()
        .unwrap()
        .to_string();
    let key_file = dir.path().join("owner.key");
    std::fs::write(&key_file, &owner).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();

    // serve on an ephemeral port; readiness is the logged address.
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
    let mut lines = BufReader::new(stderr).lines();
    let mut server = String::new();
    for line in lines.by_ref() {
        let line = line.unwrap();
        if let Ok(log) = serde_json::from_str::<Value>(&line)
            && let Some(addr) = log["fields"]["address"].as_str()
        {
            server = format!("http://{addr}");
            break;
        }
    }
    assert!(!server.is_empty(), "server never logged its address");
    std::thread::spawn(move || for _ in lines {});
    let cli = Cli { server, key_file };

    // create + seed + keyed write + byte-identical replay.
    assert_eq!(cli.ok("create", &["create", "churn"])["name"], "churn");
    cli.ok(
        "ddl",
        &[
            "query",
            "churn",
            "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)",
        ],
    );
    let first: Value = cli.ok(
        "insert",
        &[
            "query",
            "churn",
            "INSERT INTO t(v) VALUES ('a')",
            "--idempotency-key",
            "cli-k1",
        ],
    );
    let replay: Value = cli.ok(
        "replay",
        &[
            "query",
            "churn",
            "INSERT INTO t(v) VALUES ('a')",
            "--idempotency-key",
            "cli-k1",
        ],
    );
    assert_eq!(first, replay, "keyed replay bytes drifted");

    // atomic batch from file, then exact count.
    let batch_file = dir.path().join("batch.json");
    std::fs::write(
        &batch_file,
        r#"{"atomic":true,"statements":[{"sql":"INSERT INTO t(v) VALUES ('b')"},{"sql":"INSERT INTO t(v) VALUES ('c')"}]}"#,
    )
    .unwrap();
    cli.ok("batch", &["batch", "churn", batch_file.to_str().unwrap()]);
    assert_eq!(
        cli.ok("count", &["query", "churn", "SELECT count(*) FROM t"])["rows"],
        serde_json::json!([[3]])
    );

    // export to file, import it back under a new name, clone, all counts exact.
    let snap = dir.path().join("churn.sqlite");
    let exported = cli.ok(
        "export",
        &["export", "churn", "--out", snap.to_str().unwrap()],
    );
    assert_eq!(exported["name"], "churn");
    assert!(exported["size"].as_u64().unwrap() > 0);
    assert!(snap.exists(), "export wrote no file");
    assert_eq!(
        cli.ok(
            "import",
            &["import", "churn_copy", "--from", snap.to_str().unwrap()]
        )["imported"],
        "churn_copy"
    );
    assert_eq!(
        cli.ok(
            "copy count",
            &["query", "churn_copy", "SELECT count(*) FROM t"]
        )["rows"],
        serde_json::json!([[3]])
    );
    assert_eq!(
        cli.ok("clone", &["clone", "churn", "churn_clone"])["destination"],
        "churn_clone"
    );
    assert_eq!(
        cli.ok(
            "clone count",
            &["query", "churn_clone", "SELECT count(*) FROM t"]
        )["rows"],
        serde_json::json!([[3]])
    );

    // scoped key: works on its database, 403 elsewhere, 401 after revoke.
    let issued = cli.ok(
        "key create",
        &[
            "key",
            "create",
            "writer",
            "--role",
            "database_read_write",
            "--database",
            "churn",
        ],
    );
    let scoped_file = dir.path().join("writer.key");
    std::fs::write(&scoped_file, issued["key"].as_str().unwrap()).unwrap();
    std::fs::set_permissions(&scoped_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let scoped = Cli {
        server: cli.server.clone(),
        key_file: scoped_file,
    };
    assert_eq!(
        scoped.ok("scoped read", &["query", "churn", "SELECT count(*) FROM t"])["rows"],
        serde_json::json!([[3]])
    );
    scoped.err(
        "scoped cross-db",
        &["query", "churn_copy", "SELECT 1"],
        "database_forbidden",
    );
    assert_eq!(
        cli.ok("key revoke", &["key", "revoke", "writer"])["revoked"],
        true
    );
    scoped.err(
        "revoked use",
        &["query", "churn", "SELECT 1"],
        "invalid_credential",
    );

    // piped shell: use, :batch from pasted JSON, count, .tables.
    let mut shell = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            &cli.server,
            "--key-file",
            cli.key_file.to_str().unwrap(),
            "shell",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let script = "use churn\n:batch\n{\"atomic\":true,\"statements\":[{\"sql\":\"INSERT INTO t(v) VALUES ('s1')\"},{\"sql\":\"INSERT INTO t(v) VALUES ('s2')\"}]}\nSELECT count(*) FROM t;\n.tables\n";
    shell
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let shell_out = shell.wait_with_output().unwrap();
    assert!(
        shell_out.status.success(),
        "shell: {}",
        String::from_utf8_lossy(&shell_out.stderr)
    );
    let text = String::from_utf8_lossy(&shell_out.stdout).into_owned();
    assert!(text.contains('5'), "shell count missing 5 rows: {text}");
    assert!(text.contains('t'), "shell .tables missing t: {text}");
    assert_eq!(
        cli.ok(
            "post-shell count",
            &["query", "churn", "SELECT count(*) FROM t"]
        )["rows"],
        serde_json::json!([[5]])
    );

    // delete confirmation gate: absent and wrong fail locally, exact succeeds.
    cli.err(
        "delete no confirm",
        &["delete", "churn"],
        "confirmation_required",
    );
    cli.err(
        "delete wrong confirm",
        &["delete", "churn", "--confirm", "wrong"],
        "confirmation_required",
    );
    assert_eq!(
        cli.ok("delete", &["delete", "churn", "--confirm", "churn"])["deleted"],
        true
    );
    let (_, stdout, _) = cli.err(
        "query tombstoned",
        &["query", "churn", "SELECT 1"],
        "database_not_found",
    );
    assert!(
        stdout.contains("\"status\":404"),
        "tombstone query: {stdout}"
    );
    let listed = cli.ok("list", &["list"]);
    let names: Vec<String> = listed["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    assert!(
        !names.contains(&"churn".to_string()),
        "tombstoned churn still listed: {names:?}"
    );
    assert!(
        names.contains(&"churn_copy".to_string()) && names.contains(&"churn_clone".to_string()),
        "{names:?}"
    );

    child.kill().unwrap();
    child.wait().unwrap();
}
