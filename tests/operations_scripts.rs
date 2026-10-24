//! Focused contract tests for operator scripts changed by the project audit.

use serde_json::{Value, json};
use sha2::Digest;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, thread, time::Duration};

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

#[test]
fn backup_rejects_invalid_retention_before_touching_backup_root() {
    for keep in ["0", "-1", "many", ""] {
        let dir = tempdir();
        let root = dir.path().join("backups");
        let output = Command::new("bash")
            .args(["scripts/micro2-backup.sh", "--keep", keep, "--backup-root"])
            .arg(&root)
            .output()
            .expect("run backup script");
        assert_eq!(output.status.code(), Some(2), "keep={keep:?}");
        assert!(!root.exists(), "invalid retention created backup root");
    }
}

#[test]
fn backup_staging_is_unique_and_cleaned_on_failure() {
    let dir = tempdir();
    let root = dir.path().join("backups");
    let fake = dir.path().join("dg");
    fs::write(
        &fake,
        "#!/bin/sh\ncase \"$1\" in --version) echo 'dg 0.1.0';; list) echo '{\"databases\":[]}';; esac\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("bash")
        .args(["scripts/micro2-backup.sh", "--backup-root"])
        .arg(&root)
        .env("DG_BIN", &fake)
        .output()
        .expect("run backup script");
    assert!(
        !output.status.success(),
        "empty backup unexpectedly succeeded"
    );
    let leftovers = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging."))
        .count();
    assert_eq!(leftovers, 0, "failed backup leaked staging directory");
}

#[test]
fn backup_uses_array_invocation_and_emits_contract_artifacts() {
    let dir = tempdir();
    let root = dir.path().join("backup root with spaces");
    let data = dir.path().join("data with spaces");
    let key = dir.path().join("owner key with spaces");
    fs::create_dir_all(&data).unwrap();
    fs::write(&key, "a".repeat(64)).unwrap();
    let fake = dir.path().join("dg binary with spaces");
    fs::write(
        &fake,
        r#"#!/usr/bin/env bash
set -eu
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[$i]}" in
    --version) echo 'dg 0.1.0'; exit 0;;
    list) echo '{"databases":["db"]}'; exit 0;;
    export)
      out=""
      for ((j=i+1; j<${#args[@]}; j++)); do
        if [ "${args[$j]}" = "--out" ]; then out="${args[$((j+1))]}"; fi
      done
      python3 - "$out" <<'PY'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('create table payload(value integer)')
con.execute('insert into payload values (7)')
con.commit()
con.close()
PY
      exit 0;;
  esac
done
exit 2
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let metadata = rusqlite::Connection::open(data.join("metadata.sqlite")).unwrap();
    metadata
        .execute_batch("create table databases(name text); insert into databases values ('db');")
        .unwrap();
    drop(metadata);
    let output = Command::new("bash")
        .args(["scripts/micro2-backup.sh", "--data-dir"])
        .arg(&data)
        .args(["--backup-root"])
        .arg(&root)
        .args(["--key-file"])
        .arg(&key)
        .env("DG_BIN", &fake)
        .output()
        .expect("run backup script");
    assert!(
        output.status.success(),
        "backup failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let set = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_type().unwrap().is_dir()
                && entry.file_name().to_string_lossy().starts_with('2')
        })
        .expect("published set");
    let set = set.path();
    for name in [
        "manifest.json",
        "metadata.sqlite.bak",
        "db.sqlite",
        "db.sha256",
    ] {
        assert!(set.join(name).is_file(), "missing {name}");
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(set.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["databases"][0]["name"], "db");
    assert_eq!(manifest["databases"][0]["file"], "db.sqlite");
    assert_eq!(manifest["registry"]["file"], "metadata.sqlite.bak");
    let sidecar = fs::read_to_string(set.join("db.sha256")).unwrap();
    let digest = sha2::Sha256::digest(fs::read(set.join("db.sqlite")).unwrap());
    assert_eq!(sidecar.trim(), hex::encode(digest));
}

#[test]
fn failed_restore_drill_terminates_its_server() {
    let dir = tempdir();
    let pid_file = dir.path().join("server.pid");
    let fake = dir.path().join("failing dg");
    fs::write(
        &fake,
        r#"#!/usr/bin/env bash
set -eu
if [ "$1" = init ]; then
  echo '{"owner_key":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}'
  exit 0
fi
if [ "$1" = serve ]; then
  echo $$ > "$PIDFILE"
  printf '%s\n' '{"fields":{"address":"127.0.0.1:1"}}' >&2
  exec sleep 30
fi
exit 2
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("python3")
        .args(["scripts/wipe_restore.py"])
        .arg(&fake)
        .env("PIDFILE", &pid_file)
        .output()
        .expect("run failing restore drill");
    assert!(!output.status.success());
    let pid = fs::read_to_string(&pid_file).unwrap();
    let pid = pid.trim();
    for _ in 0..40 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("restore drill left server process {pid} running");
}

/// The rehearsal's contract is behavioral, and the string assertions this
/// replaces could pass with the checks deleted from the commands they name.
/// A real run against a faithful fake proves the PASS path; a fake that
/// drops the rollback generation's row must turn the same run nonzero.
/// `--fail-with-body` is additionally pinned by the string assertions so a
/// silently-ignored HTTP 500 cannot pass for a success.
#[test]
fn upgrade_rehearsal_passes_on_a_faithful_binary_and_fails_on_a_lost_generation() {
    let script = fs::read_to_string("scripts/upgrade-rehearsal.sh").unwrap();
    assert!(script.contains("--fail-with-body"));
    assert!(script.contains("rows[-1][0] == \"rollback-gen\""));
    assert_eq!(
        script.matches("rollback-generation write intact").count(),
        2
    );

    let dir = tempdir();
    let fake = dir.path().join("dg");
    fs::write(&fake, FAKE_DG).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let faithful = Command::new("bash")
        .args([
            "scripts/upgrade-rehearsal.sh",
            fake.to_str().unwrap(),
            fake.to_str().unwrap(),
        ])
        .output()
        .expect("run upgrade rehearsal");
    assert!(
        faithful.status.success(),
        "faithful rehearsal failed:\n{}\n{}",
        String::from_utf8_lossy(&faithful.stdout),
        String::from_utf8_lossy(&faithful.stderr)
    );
    assert!(String::from_utf8_lossy(&faithful.stdout).contains("UPGRADE REHEARSAL PASS"));

    // Same binary, minus the rollback generation's row: the rehearsal must
    // exit nonzero instead of printing its PASS line.
    let regressed = Command::new("bash")
        .args([
            "scripts/upgrade-rehearsal.sh",
            fake.to_str().unwrap(),
            fake.to_str().unwrap(),
        ])
        .env("FAKE_DROP_LAST_GEN", "1")
        .output()
        .expect("run regressed upgrade rehearsal");
    assert!(
        !regressed.status.success(),
        "lost rollback generation passed the rehearsal:\n{}",
        String::from_utf8_lossy(&regressed.stdout)
    );
    assert!(
        String::from_utf8_lossy(&regressed.stderr).contains("AssertionError"),
        "failure must come from the generation assertion: {}",
        String::from_utf8_lossy(&regressed.stderr)
    );
}

/// Fake `dg` for the rehearsal: a real SQLite-backed server over the exact
/// route subset the script drives, with one optional fault — drop the final
/// `%-gen` row from the SELECT that proves the rollback generation survived.
const FAKE_DG: &str = r#"#!/usr/bin/env python3
import json, os, sqlite3, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

if sys.argv[1:2] == ["init"]:
    print(json.dumps({"owner_key": "a" * 64}))
    sys.exit(0)
if sys.argv[1:2] != ["serve"]:
    sys.exit(2)
data_dir = sys.argv[sys.argv.index("--data-dir") + 1]
db_path = os.path.join(data_dir, "up.sqlite")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        path = self.path
        if path == "/v1/databases":
            return self.reply(200, {"name": body.get("name")})
        if path.endswith("/query"):
            sql = body.get("sql", "")
            conn = sqlite3.connect(db_path)
            try:
                cursor = conn.execute(sql, body.get("args") or [])
                if cursor.description is None:
                    conn.commit()
                    return self.reply(200, {"columns": [], "rows": [], "rows_affected": cursor.rowcount})
                rows = [list(row) for row in cursor.fetchall()]
                columns = [column[0] for column in cursor.description]
                if os.environ.get("FAKE_DROP_LAST_GEN") and "%-gen" in sql and rows:
                    rows = rows[:-1]
                return self.reply(200, {"columns": columns, "rows": rows})
            finally:
                conn.close()
        if path.endswith("/batch"):
            conn = sqlite3.connect(db_path)
            try:
                results = []
                for statement in body.get("statements", []):
                    cursor = conn.execute(statement.get("sql", ""), statement.get("args") or [])
                    results.append({"columns": [], "rows": [], "rows_affected": cursor.rowcount})
                conn.commit()
                return self.reply(200, {"results": results})
            finally:
                conn.close()
        if path.endswith("/export"):
            return self.reply(200, {"sha256": "0" * 64, "data_base64": "", "size": 0})
        return self.reply(404, {"error": {"code": "not_found", "message": path}})

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(json.dumps({"fields": {"address": "127.0.0.1:%d" % server.server_address[1]}}, separators=(",", ":")), flush=True)
server.serve_forever()
"#;

#[test]
fn soak_readiness_wait_is_bounded() {
    // The fake init succeeds, then the fake serve remains alive without ever
    // logging. The bounded environment override proves readiness cannot hang.
    let dir = tempdir();
    let fake = dir.path().join("dg");
    fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = init ]; then echo '{\"owner_key\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}'; exit 0; fi\nsleep 30\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("python3")
        .args(["-c", "import runpy,sys; sys.argv=['soak.py','--dg-bin',sys.argv[1],'--seconds','0','--out',sys.argv[2]]; runpy.run_path('scripts/soak.py',run_name='__main__')"])
        .arg(&fake)
        .arg(dir.path().join("soak.log"))
        .env("SOAK_READINESS_TIMEOUT", "0.2")
        .output()
        .expect("run soak script");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("server never logged"));
}

#[test]
fn setup_help_documents_interface_without_building() {
    let output = Command::new("bash")
        .args(["scripts/setup.sh", "--help"])
        .output()
        .expect("run setup script");
    assert!(output.status.success());
    let usage = String::from_utf8_lossy(&output.stderr);
    for flag in ["--prefix", "--smoke", "--no-install", "--strict", "--help"] {
        assert!(usage.contains(flag), "missing {flag}: {usage}");
    }
}

/// The staleness guard exists because the old check could not fail: it ran
/// `dg tui --help | grep -q terminal`, which passes against a build from any
/// date, including the five-day-old one an operator was actually running. So
/// pin the behaviour it was replaced with -- an install older than the source
/// tree is reported, and under --strict the script refuses to call it done.
#[test]
fn setup_flags_an_install_older_than_the_source_tree() {
    let dir = tempfile::tempdir().expect("temp dir");
    let prefix = dir.path().join("prefix");
    std::fs::create_dir_all(&prefix).expect("prefix");
    // A stand-in install that is unmistakably older than any source file.
    let stale = prefix.join("dg");
    std::fs::write(&stale, b"#!/bin/sh\necho 'dg 0.0.0-old'\n").expect("write stale dg");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stale dg");
    }
    // Backdate it so the comparison is decisive rather than racing the
    // checkout. `touch` rather than a new dependency for one timestamp.
    let touched = Command::new("touch")
        .args(["-d", "2020-01-01T00:00:00Z"])
        .arg(&stale)
        .status()
        .expect("run touch");
    assert!(touched.success(), "could not backdate the staged dg");

    // Warn mode: says so, and still exits 0 (the install is usable, just old).
    let warn = Command::new("bash")
        .args(["scripts/setup.sh", "--no-install", "--prefix"])
        .arg(&prefix)
        .output()
        .expect("run setup in warn mode");
    let warn_text = format!(
        "{}{}",
        String::from_utf8_lossy(&warn.stdout),
        String::from_utf8_lossy(&warn.stderr)
    );
    assert!(
        warn_text.contains("STALE"),
        "warn mode did not report the stale install: {warn_text}"
    );
    assert!(
        warn.status.success(),
        "warn mode must not fail on a stale install: {warn_text}"
    );

    // Strict mode: same finding, non-zero exit, so CI can gate on it.
    let strict = Command::new("bash")
        .args(["scripts/setup.sh", "--no-install", "--strict", "--prefix"])
        .arg(&prefix)
        .output()
        .expect("run setup in strict mode");
    let strict_text = format!(
        "{}{}",
        String::from_utf8_lossy(&strict.stdout),
        String::from_utf8_lossy(&strict.stderr)
    );
    assert!(
        strict_text.contains("STALE"),
        "strict mode did not report the stale install: {strict_text}"
    );
    assert!(
        !strict.status.success(),
        "strict mode must fail on a stale install: {strict_text}"
    );
}

#[test]
fn setup_rejects_unknown_arguments_before_doing_anything() {
    let output = Command::new("bash")
        .args(["scripts/setup.sh", "--deploy-production"])
        .output()
        .expect("run setup script");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown argument"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The install used to fail with a bare "Text file busy" whenever a `dg` was
/// running -- which is exactly when an operator is most likely to install
/// one: their own view is on screen. The installer now replaces the PATH
/// instead of the inode, so the running process keeps the code it started
/// with, and the next launch gets the new binary.
///
/// The precondition is asserted rather than assumed: a plain `cp` over the
/// running binary has to fail, so this test cannot pass by never reaching
/// the busy-file case it exists to cover.
#[test]
fn an_install_replaces_a_running_binary_without_disturbing_it() {
    let dir = tempdir();
    let prefix = dir.path().join("prefix");
    fs::create_dir_all(&prefix).expect("prefix");
    let installed = prefix.join("dg");

    // A real executable, so the kernel holds its inode open for execution.
    fs::copy(env!("CARGO_BIN_EXE_dg"), &installed).expect("copy dg");
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).expect("chmod dg");

    // Run it, so that inode is being executed rather than merely present.
    let mut server = Command::new(&installed)
        .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
        .arg(dir.path().join("data"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the running dg");

    // Wait until it is genuinely executing that file, not merely spawned.
    let exe = Path::new("/proc").join(server.id().to_string()).join("exe");
    let mut executing = false;
    for _ in 0..100 {
        if fs::read_link(&exe).map(|p| p == installed).unwrap_or(false) {
            executing = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        executing,
        "the dg under test never started executing {installed:?}"
    );

    // The precondition: writing OVER a running executable is refused. If
    // this ever succeeds, the busy-file case is no longer reachable and
    // the rest of the test would prove nothing.
    let source = dir.path().join("new-dg");
    fs::write(&source, b"#!/bin/sh\necho 'dg 9.9.9-new'\n").expect("write new dg");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("chmod new dg");

    let blind = Command::new("cp")
        .arg(&source)
        .arg(&installed)
        .output()
        .expect("run cp");
    assert!(
        !blind.status.success(),
        "a plain cp over the running binary succeeded, so this test is not \
         exercising the busy-file case it exists to cover"
    );

    // The install: a different binary, placed over the running one.
    let output = Command::new("bash")
        .args(["scripts/install-binary.sh"])
        .arg(&source)
        .arg(&installed)
        .output()
        .expect("run install-binary.sh");
    assert!(
        output.status.success(),
        "the install failed over a running binary: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The path now holds the new binary.
    let installed_text = fs::read_to_string(&installed).expect("read installed dg");
    assert!(
        installed_text.contains("dg 9.9.9-new"),
        "the path still holds the old binary: {installed_text}"
    );

    // And the running process was not disturbed: same pid, still alive,
    // still holding its own inode -- and that inode is no longer the file
    // at the install path, because the path now holds the new binary.
    //
    // How the old inode is disposed of varies: a plain unlink leaves the
    // link pointing at a deleted file, and a recoverable-delete `rm`
    // moves it aside first. Both leave the running process on the inode
    // it started with, which is the property that matters.
    assert!(
        server.try_wait().expect("poll the running dg").is_none(),
        "the install killed the running dg"
    );
    let exe_after = fs::read_link(&exe).unwrap_or_else(|e| panic!("read {exe:?}: {e}"));
    assert_ne!(
        exe_after, installed,
        "the running dg still points at the live path {exe_after:?}, so the \
         install did not replace the path under it"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// A `dg` stub that answers `list` with the given names.
fn fake_dg(dir: &Path, databases: &[&str]) -> std::path::PathBuf {
    let path = dir.join("dg");
    let names = format!("{{\"databases\":[{}]}}", {
        let quoted: Vec<String> = databases.iter().map(|name| format!("\"{name}\"")).collect();
        quoted.join(",")
    });
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --server) shift 2;;\n\
             --key-file) shift 2;;\n\
             esac\n\
             case \"$1\" in\n\
             list) echo '{names}';;\n\
             *) echo '{{}}';;\n\
             esac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Run the litestream sync script against a stubbed gateway.
///
/// The script is what knows where the off-host copy is, so it is what
/// publishes it; the gateway only reads what was published.
fn run_sync(dir: &Path, env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new("bash");
    command.arg("scripts/litestream-sync.sh");
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("DG_LIST_AS", "");
    command.env("LITESTREAM_RESTART_CMD", "true");
    command.env("LITESTREAM_CONFIG_OWNER", "");
    command.env("HOME", dir);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run litestream-sync.sh")
}

#[test]
fn the_sync_publishes_where_the_off_host_copy_is() {
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    let config = dir.path().join("litestream.yml");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    fs::write(data.join("alpha.sqlite"), b"sqlite").unwrap();
    fs::write(data.join("beta.sqlite"), b"sqlite").unwrap();
    let dg = fake_dg(dir.path(), &["alpha", "beta"]);

    let output = run_sync(
        dir.path(),
        &[
            ("DG_BIN", dg.to_str().unwrap()),
            (
                "DG_KEY_FILE",
                dir.path().join("owner.key").to_str().unwrap(),
            ),
            ("DG_DATA_DIR", data.to_str().unwrap()),
            ("LITESTREAM_CONFIG", config.to_str().unwrap()),
            ("DG_BACKUP_ROOT", backups.to_str().unwrap()),
            ("DG_REPLICA_URL_TEMPLATE", "file:///srv/replicas/{name}"),
        ],
    );
    assert!(
        output.status.success(),
        "the sync failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The published status is what the gateway reports when an operator
    // asks where their data is.
    let published = backups.join("replica.json");
    assert!(
        published.exists(),
        "the sync published no replica status: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&published).unwrap()).unwrap();
    assert_eq!(
        record["target"],
        serde_json::json!("file:///srv/replicas/{name}")
    );
    assert!(
        record["bucket"].is_null(),
        "a file replica has no bucket, and saying otherwise would be a lie: {record}"
    );
    let covered: Vec<String> = record["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_string())
        .collect();
    assert!(covered.contains(&"alpha".to_string()), "{covered:?}");
    assert!(covered.contains(&"beta".to_string()), "{covered:?}");
    assert!(
        record["published_at"].as_u64().unwrap_or(0) > 0,
        "the published status has no timestamp: {record}"
    );
}

#[test]
fn the_published_replica_status_never_carries_a_credential() {
    // The publisher names a place: the bucket, the endpoint, the region.
    // A credential in that file would be a credential in a file the
    // gateway reads and an operator can ask for.
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    let config = dir.path().join("litestream.yml");
    let env_file = dir.path().join("litestream.env");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    fs::write(data.join("alpha.sqlite"), b"sqlite").unwrap();
    fs::write(
        &env_file,
        "LITESTREAM_S3_BUCKET=dg-sandbox\n\
         LITESTREAM_S3_ENDPOINT=s3.example.invalid\n\
         LITESTREAM_S3_REGION=gra\n\
         LITESTREAM_S3_ACCESS_KEY_ID=[DRACON_SECRET:REDACTED]\n\
         LITESTREAM_S3_SECRET_ACCESS_KEY=placeholder-secret-not-published\n",
    )
    .unwrap();
    let dg = fake_dg(dir.path(), &["alpha"]);

    let output = run_sync(
        dir.path(),
        &[
            ("DG_BIN", dg.to_str().unwrap()),
            (
                "DG_KEY_FILE",
                dir.path().join("owner.key").to_str().unwrap(),
            ),
            ("DG_DATA_DIR", data.to_str().unwrap()),
            ("LITESTREAM_CONFIG", config.to_str().unwrap()),
            ("LITESTREAM_ENV_FILE", env_file.to_str().unwrap()),
            ("DG_BACKUP_ROOT", backups.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "the sync failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let published = backups.join("replica.json");
    assert!(published.exists(), "the sync published no replica status");
    let text = fs::read_to_string(&published).unwrap();
    assert!(
        text.contains("dg-sandbox"),
        "the published status does not name the bucket: {text}"
    );
    assert!(
        !text.contains("[DRACON_SECRET:REDACTED]"),
        "the published status carries the access key: {text}"
    );
    assert!(
        !text.contains("placeholder-secret-not-published"),
        "the published status carries the secret key: {text}"
    );
    // The rendered config keeps the credential references, so the
    // sidecar still finds them in its environment file.
    let rendered = fs::read_to_string(&config).unwrap();
    assert!(
        !rendered.contains("[DRACON_SECRET:REDACTED]"),
        "the rendered config inlined the access key: {rendered}"
    );
}

#[test]
fn the_restore_drill_publishes_whether_the_copies_restored() {
    // A backup that was never restored is an assumption. The drill is what
    // turns it into evidence, so its result has to be published where the
    // gateway can report it -- and published whether it passed or failed.
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    // The live database the drill restores and compares against.
    {
        let db = rusqlite::Connection::open(data.join("alpha.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1),(2),(3);",
        )
        .unwrap();
    }
    // The registry the drill restores too: it must name the live
    // databases and carry the credentials, or the copies are files that
    // no key can open.
    {
        let registry = rusqlite::Connection::open(data.join("metadata.sqlite")).unwrap();
        registry
            .execute_batch(
                "CREATE TABLE databases (name TEXT);\
                 CREATE TABLE credentials (id TEXT);\
                 INSERT INTO databases VALUES ('alpha');\
                 INSERT INTO credentials VALUES ('owner');",
            )
            .unwrap();
    }
    // A litestream that restores by copying: the drill's bar is the
    // comparison against the live gateway, not the tool.
    let litestream = dir.path().join("litestream");
    fs::write(
        &litestream,
        "#!/bin/sh\n\
         out=\"\"; src=\"\"\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in -config) shift 2;; -o) out=\"$2\"; shift 2;; *) src=\"$1\"; shift;; esac\n\
         done\n\
         cp \"$src\" \"$out\"\n",
    )
    .unwrap();
    fs::set_permissions(&litestream, fs::Permissions::from_mode(0o755)).unwrap();
    // A gateway that answers the list and the counts the drill compares.
    let dg = dir.path().join("dg");
    fs::write(
        &dg,
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in --server) shift 2;; --key-file) shift 2;; *) break;; esac\n\
         done\n\
         cmd=\"$1\"; shift\n\
         case \"$cmd\" in\n\
           list) echo '{\"databases\":[\"alpha\"]}';;\n\
           query) case \"$2\" in *sqlite_master*) echo '{\"rows\":[[\"t\"]]}';; *) echo '{\"rows\":[[3]]}';; esac;;\n\
           *) echo '{}';;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&dg, fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("litestream.yml");
    fs::write(&config, "dbs: []\n").unwrap();

    let output = Command::new("bash")
        .arg("scripts/bucket-restore-drill.sh")
        .env("DG_BIN", &dg)
        .env("DG_KEY_FILE", dir.path().join("owner.key"))
        .env("DG_DATA_DIR", &data)
        .env("LITESTREAM_BIN", &litestream)
        .env("LITESTREAM_CONFIG", &config)
        .env("LITESTREAM_ENV_FILE", dir.path().join("no-such.env"))
        .env("DG_BACKUP_ROOT", &backups)
        .env("DG_DRILL_SCRATCH_PARENT", dir.path())
        .output()
        .expect("run the restore drill");
    assert!(
        output.status.success(),
        "the drill failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let published = backups.join("restore-drill.json");
    assert!(
        published.exists(),
        "the drill published no result: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&published).unwrap()).unwrap();
    assert_eq!(
        record["outcome"].as_str(),
        Some("ok"),
        "the drill did not pass: {record}"
    );
    assert_eq!(
        record["databases"].as_u64(),
        Some(1),
        "the drill checked no database: {record}"
    );
    assert!(
        record["drilled_at"].as_u64().unwrap_or(0) > 0,
        "the published result has no timestamp: {record}"
    );
}

/// A litestream stub that answers the two commands the restore-point
/// script makes: `ltx` (what the replica holds) and `restore` (write a
/// copy out).
fn fake_litestream(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("litestream");
    fs::write(
        &path,
        "#!/bin/sh\n\
         case \"$1\" in\n\
         ltx)\n\
         echo 'level  min_txid          max_txid          size  created'\n\
         echo '9      0000000000000001  0000000000000001  893   2026-09-15T14:16:08Z'\n\
         exit 0;;\n\
         restore)\n\
         out=\"\"\n\
         while [ $# -gt 0 ]; do\n\
         case \"$1\" in\n\
         -o) out=\"$2\"; shift 2;;\n\
         *) shift;;\n\
         esac\n\
         done\n\
         [ -n \"$out\" ] || exit 2\n\
         python3 -c \"import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('create table t(id integer, name text)'); c.execute(\\\"insert into t values (1,'row-1')\\\"); c.commit(); c.close()\" \"$out\"\n\
         exit 0;;\n\
         esac\n\
         exit 1\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Run the restore-point script against the stubs.
fn run_restore_point(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new("bash");
    command.arg("scripts/restore-point.sh");
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", dir);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run restore-point.sh")
}

/// The litestream config the script reads the database list from.
fn litestream_config(dir: &Path, data: &Path, names: &[&str]) -> std::path::PathBuf {
    let config = dir.join("litestream.yml");
    let mut text = String::from("dbs:\n");
    for name in names {
        text.push_str(&format!("  - path: {}/{name}.sqlite\n", data.display()));
        text.push_str(&format!(
            "    replicas:\n      - url: s3://dg-sandbox/{name}\n        access-key-id: ${{LITESTREAM_S3_ACCESS_KEY_ID}}\n        secret-access-key: ${{LITESTREAM_S3_SECRET_ACCESS_KEY}}\n"
        ));
    }
    fs::write(&config, text).unwrap();
    config
}

#[test]
fn the_restore_point_script_publishes_the_window_the_replica_offers() {
    // "There is a backup" is a claim; "you can go back to any moment
    // between these two" is the useful one. Only the host knows the
    // window, so the host publishes it and the gateway reports it.
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    let restores = dir.path().join("restores");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    fs::write(data.join("alpha.sqlite"), b"sqlite").unwrap();
    let litestream = fake_litestream(dir.path());
    let config = litestream_config(dir.path(), &data, &["alpha"]);
    let absent_env = dir.path().join("absent.env");

    let output = run_restore_point(
        dir.path(),
        &["--list"],
        &[
            ("LITESTREAM_BIN", litestream.to_str().unwrap()),
            ("LITESTREAM_CONFIG", config.to_str().unwrap()),
            (
                "LITESTREAM_ENV_FILE",
                dir.path().join("absent.env").to_str().unwrap(),
            ),
            ("DG_DATA_DIR", data.to_str().unwrap()),
            ("DG_BACKUP_ROOT", backups.to_str().unwrap()),
            ("RESTORE_ROOT", restores.to_str().unwrap()),
            ("DG_LITESTREAM_STATUS_CMD", "echo active"),
        ],
    );
    assert!(
        output.status.success(),
        "the script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let published: Value =
        serde_json::from_str(&fs::read_to_string(backups.join("restore-points.json")).unwrap())
            .unwrap();
    assert_eq!(published["window_start"], json!("2026-09-15T14:16:08Z"));
    assert_eq!(published["window_end"], json!("2026-09-15T14:16:08Z"));
    assert_eq!(published["sidecar"], json!("active"));
    assert_eq!(published["databases"]["alpha"]["files"], json!(1));
    assert_eq!(published["databases"]["alpha"]["bytes"], json!(893));
}

#[test]
fn the_restore_point_script_publishes_what_a_restore_produced() {
    // A window nobody has ever exercised is a promise. A restore that
    // was taken, verified and published is evidence.
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    let restores = dir.path().join("restores");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    fs::write(data.join("alpha.sqlite"), b"sqlite").unwrap();
    let litestream = fake_litestream(dir.path());
    let config = litestream_config(dir.path(), &data, &["alpha"]);
    let absent_env = dir.path().join("absent.env");

    let output = run_restore_point(
        dir.path(),
        &[
            "--apply",
            "--to",
            "2026-09-20T00:00:00Z",
            "--database",
            "alpha",
        ],
        &[
            ("LITESTREAM_BIN", litestream.to_str().unwrap()),
            ("LITESTREAM_CONFIG", config.to_str().unwrap()),
            (
                "LITESTREAM_ENV_FILE",
                dir.path().join("absent.env").to_str().unwrap(),
            ),
            ("DG_DATA_DIR", data.to_str().unwrap()),
            ("DG_BACKUP_ROOT", backups.to_str().unwrap()),
            ("RESTORE_ROOT", restores.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "the script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let published: Value =
        serde_json::from_str(&fs::read_to_string(backups.join("restore-results.json")).unwrap())
            .unwrap();
    assert_eq!(published["outcome"], json!("ok"));
    assert_eq!(published["database"], json!("alpha"));
    assert_eq!(published["timestamp"], json!("2026-09-20T00:00:00Z"));
    assert_eq!(published["integrity"], json!("ok"));
    assert_eq!(published["tables"], json!(1));
    assert_eq!(published["rows"], json!(1));
    // The restored copy exists, and it is not over a live database.
    let path = published["path"].as_str().unwrap();
    assert!(
        Path::new(path).is_file(),
        "the restored copy is not where the result says it is: {path}"
    );
    assert!(
        !path.starts_with(data.to_str().unwrap()),
        "a restore wrote over the live data directory: {path}"
    );
}

#[test]
fn the_published_restore_facts_never_carry_a_credential() {
    // The bucket's credentials live in one file on the host and are
    // never published: what is published names a place, it does not open
    // one.
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    let restores = dir.path().join("restores");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    fs::write(data.join("alpha.sqlite"), b"sqlite").unwrap();
    let litestream = fake_litestream(dir.path());
    let config = litestream_config(dir.path(), &data, &["alpha"]);
    let env_file = dir.path().join("litestream.env");
    let absent_env = dir.path().join("absent.env");
    fs::write(
        &env_file,
        "LITESTREAM_S3_ACCESS_KEY_ID=[DRACON_SECRET:REDACTED]KEY\n\
         LITESTREAM_S3_SECRET_ACCESS_KEY=placeholder-secret-not-published\n",
    )
    .unwrap();

    let output = run_restore_point(
        dir.path(),
        &["--list"],
        &[
            ("LITESTREAM_BIN", litestream.to_str().unwrap()),
            ("LITESTREAM_CONFIG", config.to_str().unwrap()),
            ("LITESTREAM_ENV_FILE", env_file.to_str().unwrap()),
            ("DG_DATA_DIR", data.to_str().unwrap()),
            ("DG_BACKUP_ROOT", backups.to_str().unwrap()),
            ("RESTORE_ROOT", restores.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "the script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let published = fs::read_to_string(backups.join("restore-points.json")).unwrap();
    assert!(
        !published.contains("[DRACON_SECRET:REDACTED]KEY"),
        "the published window carries the access key: {published}"
    );
    assert!(
        !published.contains("placeholder-secret-not-published"),
        "the published window carries the secret key: {published}"
    );
}

#[test]
fn the_restore_point_script_refuses_a_moment_it_cannot_restore() {
    // A timestamp that is not a moment is refused before the bucket is
    // touched, and a name that is not a database name is refused before
    // it is used to build a path.
    let dir = tempdir();
    let data = dir.path().join("data");
    let backups = dir.path().join("backups");
    let restores = dir.path().join("restores");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&backups).unwrap();
    fs::write(data.join("alpha.sqlite"), b"sqlite").unwrap();
    let litestream = fake_litestream(dir.path());
    let config = litestream_config(dir.path(), &data, &["alpha"]);
    let absent_env = dir.path().join("absent.env");
    let env = [
        ("LITESTREAM_BIN", litestream.to_str().unwrap()),
        ("LITESTREAM_CONFIG", config.to_str().unwrap()),
        ("LITESTREAM_ENV_FILE", absent_env.to_str().unwrap()),
        ("DG_DATA_DIR", data.to_str().unwrap()),
        ("DG_BACKUP_ROOT", backups.to_str().unwrap()),
        ("RESTORE_ROOT", restores.to_str().unwrap()),
    ];

    let bad_moment = run_restore_point(
        dir.path(),
        &["--apply", "--to", "yesterday", "--database", "alpha"],
        &env,
    );
    assert!(
        !bad_moment.status.success(),
        "a timestamp that is not a moment was accepted"
    );

    let bad_name = run_restore_point(
        dir.path(),
        &[
            "--apply",
            "--to",
            "2026-09-20T00:00:00Z",
            "--database",
            "../../etc/passwd",
        ],
        &env,
    );
    assert!(
        !bad_name.status.success(),
        "a path in a database name was accepted"
    );
    // Nothing was published, because nothing was attempted.
    assert!(
        !backups.join("restore-results.json").exists(),
        "a refused restore published a result anyway"
    );
}
