//! Contract tests for the orphan/residue guard (scripts/check-orphans.sh).
//!
//! The guard exists because six leaked `dg serve` processes and 277 MB of
//! scratch directories accumulated on the maintainer's box, none of them from
//! a bug in the gateway's own logic: launchers died without reaping their
//! children. The behaviour worth protecting is therefore narrow and
//! mechanical:
//!
//!   * a genuinely orphaned `dg serve` is REPORTED (non-zero exit), and
//!   * a server that is still owned by a live parent is NOT reported, because
//!     a running test suite must be able to run the guard without failing on
//!     its own server, and
//!   * the scan never kills anything.
//!
//! The fixtures below are disposable: each test starts its own server in its
//! own temp directory and reaps it itself. Nothing here touches the repo's
//! state, a real data directory, or a process the test did not start.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A `dg serve` started for one test, always reaped by `Drop`.
struct Planted {
    child: Child,
    _dir: tempfile::TempDir,
    pid: u32,
}

impl Drop for Planted {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a server that will NOT die with its parent (`--detach-safe`), so the
/// test controls exactly when it becomes an orphan.
fn plant_server() -> Planted {
    let dir = tempfile::tempdir().expect("temp dir");
    let data = dir.path().join("data");
    let init = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(init.status.success(), "init failed");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--detach-safe",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let pid = child.id();

    // Wait for the listening line so the process is fully up.
    let stderr = child.stderr.take().expect("stderr");
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
    let mut up = false;
    for _ in 0..200 {
        match lines.next() {
            Some(Ok(line)) => {
                if line.contains("listening") {
                    up = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(up, "server never logged its listen address");
    // Drain the rest so a chatty server never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    Planted {
        child,
        _dir: dir,
        pid,
    }
}

/// Run the guard, returning (success, combined output).
fn run_guard(extra: &[&str]) -> (bool, String) {
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/check-orphans.sh"))
        .args(extra)
        .current_dir(repo_root())
        .env("DG_ORPHAN_MIN_AGE_SECS", "0")
        .output()
        .expect("run check-orphans");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn a_server_owned_by_a_live_parent_is_not_reported_as_an_orphan() {
    let planted = plant_server();
    let (_ok, output) = run_guard(&[]);
    // The planted server's parent is this test binary, which is alive, so the
    // guard must not call it an orphan. Other residue may legitimately exist
    // on the machine, so assert on THIS pid only.
    let needle = format!("pid {}", planted.pid);
    assert!(
        !output.contains(&needle),
        "the guard reported a server whose parent is alive (pid {}):\n{output}",
        planted.pid
    );
}

#[test]
fn the_guard_reports_an_orphaned_server_and_never_kills_it() {
    let mut planted = plant_server();

    // Orphan a server for real: a launcher that starts it and then exits. The
    // server runs with --detach-safe so the kernel does not reap it, and the
    // kernel reparents it to init/systemd -- the exact shape of the six leaks
    // this guard was written for.
    let dir = planted._dir.path();
    let data = dir.join("data2");
    let init = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(init.status.success());
    let pidfile = dir.join("srv.pid");
    // A launcher that backgrounds the server and then sleeps briefly: when it
    // exits, the server (started with --detach-safe) is orphaned.
    let launcher = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "{} serve --detach-safe --listen 127.0.0.1:0 --data-dir {} >/dev/null 2>&1 & echo $! > {}",
            env!("CARGO_BIN_EXE_dg"),
            data.to_str().unwrap(),
            pidfile.to_str().unwrap()
        ))
        .status()
        .expect("launcher");
    assert!(launcher.success());

    let orphan_pid: u32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile")
        .trim()
        .parse()
        .expect("pid");
    // Give the kernel a moment to reparent it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && process_alive(orphan_pid) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        process_alive(orphan_pid),
        "the launcher's server should be running"
    );
    // Reaped on drop, so a failing assertion below cannot leak it.
    let _reap = ReapOnDrop(orphan_pid);

    let (ok, output) = run_guard(&[]);
    assert!(
        !ok,
        "the guard must exit non-zero when an orphaned server is running:\n{output}"
    );
    assert!(
        output.contains(&format!("orphaned dg serve: pid {orphan_pid}")),
        "the guard must name the orphaned pid {orphan_pid}:\n{output}"
    );
    // Reporting is the contract; killing is not. The server must still be up.
    assert!(
        process_alive(orphan_pid),
        "the guard killed a process it only had to report"
    );

    planted.child.kill().ok();
}

fn process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Reap a pid on drop, including when an assertion above panics.
///
/// The first draft of this test cleaned up with a plain `kill -TERM` at the end
/// of the function, and a failing assertion skipped it -- leaving exactly the
/// kind of orphaned server the guard exists to catch, planted by the guard's
/// own test. A `Drop` runs on unwind, so a test that fails still cleans up.
struct ReapOnDrop(u32);

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.to_string()])
            .status();
    }
}
