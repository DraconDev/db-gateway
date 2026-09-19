//! Shared harness for the dummy project fleet tests.
//!
//! Every fleet test drives a realistic per-project database through the
//! real `dg` binary only (init → serve → create → seed → workload →
//! exact asserts). Zero direct HTTP: this module spawns processes and
//! asserts on argv/stdout/stderr/exit codes only.

use std::{
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
};

use serde_json::Value;

pub struct Fleet {
    pub cli: Cli,
    child: Option<Child>,
    _dir: tempfile::TempDir,
}

pub struct Cli {
    pub server: String,
    pub key_file: std::path::PathBuf,
}

impl Cli {
    /// Run a `dg` subcommand; returns (exit_success, stdout, stderr).
    pub fn run(&self, args: &[&str]) -> (bool, String, String) {
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

    pub fn ok(&self, label: &str, args: &[&str]) -> Value {
        let (success, stdout, stderr) = self.run(args);
        assert!(success, "{label} failed: stdout={stdout} stderr={stderr}");
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("{label} not JSON: {stdout}"))
    }

    /// Assert a `dg` subcommand fails with the given error code.
    /// (Shared harness: only some fleet tests exercise error paths.)
    #[allow(dead_code)]
    pub fn err(&self, label: &str, args: &[&str], code: &str) -> (bool, String, String) {
        let (success, stdout, stderr) = self.run(args);
        assert!(!success, "{label} unexpectedly succeeded: {stdout}");
        assert!(
            stdout.contains(&format!("\"code\":\"{code}\""))
                || stderr.contains(&format!("\"code\":\"{code}\"")),
            "{label} missing code {code}: stdout={stdout} stderr={stderr}"
        );
        (success, stdout, stderr)
    }

    /// Run one SQL statement; returns the `rows` value.
    pub fn q(&self, label: &str, db: &str, sql: &str) -> Value {
        self.ok(label, &["query", db, sql])["rows"].clone()
    }

    /// Run one keyed SQL statement; returns the full response value.
    pub fn qkey(&self, label: &str, db: &str, sql: &str, key: &str) -> Value {
        self.ok(label, &["query", db, sql, "--idempotency-key", key])
    }
}

impl Fleet {
    /// `dg init` into a temp data dir, then `dg serve` on an ephemeral port.
    pub fn boot() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");

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

        Fleet {
            cli: Cli { server, key_file },
            child: Some(child),
            _dir: dir,
        }
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
