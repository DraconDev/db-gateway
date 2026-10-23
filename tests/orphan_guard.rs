//! Contract tests for the orphan/residue guard (scripts/check-orphans.sh).
//!
//! The guard exists because six leaked `dg serve` processes and 277 MB of
//! scratch directories accumulated on the maintainer's box, none of them from
//! a bug in the gateway's own logic: launchers died without reaping their
//! children. Two mechanisms are tested here, and they answer different halves
//! of that:
//!
//!   * `dg serve` arms `PR_SET_PDEATHSIG`, so a server started by a test
//!     binary is killed by the kernel when that binary dies -- including
//!     SIGKILL, where no `Drop` ever runs. `a_server_started_normally_dies_
//!     with_a_sigkilled_launcher` proves that against a real SIGKILL.
//!   * the guard reports an orphan that exists anyway. Its own fixture has to
//!     survive a SIGKILLed test binary, so the launcher it uses is a
//!     self-terminating watchdog: it is parented to the test, notices when it
//!     is reparented, and kills the server it started before exiting.
//!
//! Everything here is disposable: each test starts its own server in its own
//! temp directory. Nothing touches the repo, a real data directory, or a
//! process the test did not start.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Wait until `pid` is gone, or the deadline passes. Returns whether it died.
fn wait_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_alive(pid)
}

/// Kill on drop, for the paths a panic can still reach.
struct ReapOnDrop(u32);

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.to_string()])
            .status();
    }
}

/// A `dg serve` started by this test, always reaped by `Drop` on the happy and
/// panicking paths. SIGKILL of the test binary is covered by PDEATHSIG instead
/// (see `a_server_started_normally_dies_with_a_sigkilled_launcher`).
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

fn init_dir(dir: &std::path::Path) -> std::path::PathBuf {
    let data = dir.join("data");
    let out = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["init", "--data-dir", data.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    data
}

/// Start a server the way every other test in this repo does -- no
/// `--detach-safe` -- so it carries the production parent-death behaviour.
fn plant_server() -> Planted {
    let dir = tempfile::tempdir().expect("temp dir");
    let data = init_dir(dir.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            data.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let pid = child.id();
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
    std::thread::spawn(move || for _ in lines {});
    Planted {
        child,
        _dir: dir,
        pid,
    }
}

/// Run the guard, returning (success, combined output).
///
/// `DG_ORPHAN_MIN_AGE_SECS=0` because the fixture is seconds old by design.
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

/// Start a server that is genuinely orphaned (its launcher has exited).
///
/// Two properties must hold at once, and they pull in opposite directions:
///
///   * the server must be a real ORPHAN (reparented to init/systemd) so the
///     guard has something genuine to report -- which needs `--detach-safe`,
///     since PDEATHSIG would otherwise reap it; and
///   * the fixture must not survive a SIGKILL of THIS test binary, where no
///     `Drop` runs.
///
/// So there are two processes. The launcher starts the server, records the pid
/// and exits immediately -- which is what makes the server an orphan. A
/// separate detached killer then polls this test's pid and reaps the orphan
/// when it disappears, however it died. Verified against a real SIGKILL: the
/// server is reparented (ppid == systemd's pid) and the killer still removes
/// it. The launcher and killer both run with all three stdio streams
/// redirected to /dev/null, so neither holds the test's pipes open.
fn plant_orphan(dir: &std::path::Path) -> u32 {
    let data = init_dir(dir);
    let pidfile = dir.join("srv.pid");
    let launcher = dir.join("orphan-maker.sh");
    std::fs::write(
        &launcher,
        format!(
            r#"#!/usr/bin/env bash
set -uo pipefail
dg={dg}; data={data}; pidfile={pidfile}; testpid=$1
"$dg" serve --detach-safe --listen 127.0.0.1:0 --data-dir "$data" >/dev/null 2>&1 &
server=$!
printf '%s' "$server" > "$pidfile"
# Detached killer: if the test binary dies -- normally or by SIGKILL -- reap
# the orphan we just created. This is what a Drop impl cannot do.
setsid bash -c '
  testpid=$1; server=$2
  while kill -0 "$testpid" 2>/dev/null; do sleep 0.2; done
  kill -TERM "$server" 2>/dev/null
' _ "$testpid" "$server" </dev/null >/dev/null 2>&1 &
disown
exit 0
"#,
            dg = env!("CARGO_BIN_EXE_dg"),
            data = data.to_str().unwrap(),
            pidfile = pidfile.to_str().unwrap(),
        ),
    )
    .expect("write orphan maker");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Hand the launcher our own pid and let it exit; `status()` waits for it,
    // which also means the server is already reparented when this returns.
    let status = Command::new(&launcher)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run orphan maker");
    assert!(status.success(), "the orphan maker failed");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = text.trim().parse::<u32>()
            && process_alive(pid)
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the orphan maker never started a server");
}

/// Objection closed: a server spawned the ORDINARY way (no `--detach-safe`) is
/// SIGKILL-safe through `PR_SET_PDEATHSIG`, which is what makes every existing
/// `Drop`-based helper in `tests/` safe even when the test binary is killed.
#[test]
fn a_server_started_normally_dies_with_a_sigkilled_launcher() {
    let dir = tempfile::tempdir().expect("temp dir");
    let data = init_dir(dir.path());
    let pidfile = dir.path().join("srv.pid");
    let launcher_pidfile = dir.path().join("launcher.pid");
    // A launcher that starts dg serve normally (PDEATHSIG armed), records both
    // pids, then sleeps so it can be SIGKILLed while the server is up.
    //
    // The launcher records ITS OWN pid because that is the pid that must be
    // killed. `setsid` is not guaranteed to exec in place: when it forks, the
    // child pid it returns from `spawn()` is a short-lived `setsid` process,
    // and the launcher -- the shell that is the server's actual parent --
    // keeps running. Killing the `setsid` pid therefore leaves the parent
    // alive, PDEATHSIG correctly does not fire, and the assertion fails on a
    // loaded box. That is a defect in the test, not in the server.
    let launcher = dir.path().join("launcher.sh");
    std::fs::write(
        &launcher,
        format!(
            "#!/usr/bin/env bash\nprintf '%s' \"$$\" > {launcher_pidfile}\n\"{dg}\" serve --listen 127.0.0.1:0 --data-dir {data} >/dev/null 2>&1 &\nprintf '%s' \"$!\" > {pidfile}\nsleep 60\n",
            dg = env!("CARGO_BIN_EXE_dg"),
            data = data.to_str().unwrap(),
            pidfile = pidfile.to_str().unwrap(),
            launcher_pidfile = launcher_pidfile.to_str().unwrap()
        ),
    )
    .expect("write launcher");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut child = Command::new("setsid")
        .arg(&launcher)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn launcher");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut server = None;
    while Instant::now() < deadline && server.is_none() {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = text.trim().parse::<u32>()
            && process_alive(pid)
        {
            server = Some(pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let server = server.expect("server never started");
    let _reap = ReapOnDrop(server);

    // SIGKILL the launcher itself -- the process that is the server's parent.
    // Where `setsid` forked, that is not the pid `spawn()` handed back.
    let launcher_pid = std::fs::read_to_string(&launcher_pidfile)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or_else(|| child.id());
    let _ = Command::new("kill")
        .args(["-9", &launcher_pid.to_string()])
        .status();
    let _ = child.wait();

    assert!(
        wait_gone(server, Duration::from_secs(10)),
        "a normally-started server must die when its launcher is SIGKILLed (pid {server} survived); \
         this is the whole point of PR_SET_PDEATHSIG"
    );
}

/// Objection closed: the guard exits 0 on a clean probe BEFORE anything is
/// planted, and non-zero once an orphan exists. The clean result is asserted,
/// not discarded.
#[test]
fn the_guard_is_clean_first_and_reports_a_planted_orphan_second() {
    let dir = tempfile::tempdir().expect("temp dir");

    // --- clean probe: nothing planted yet -------------------------------
    // The guard inspects the live machine, so something unrelated could
    // legitimately be wrong; that is reported, not hidden. What this stage
    // asserts is narrower and honest: the guard runs, and it does NOT report
    // an orphan. If it fails here for residue reasons, say so rather than
    // pretending the probe passed.
    let (clean_ok, clean_out) = run_guard(&[]);
    // The clean line reads "no orphaned dg serve", so match the report shape
    // ("orphaned dg serve: pid N") rather than the bare phrase.
    assert!(
        !clean_out.contains("orphaned dg serve: pid "),
        "the clean probe must find no orphaned server:\n{clean_out}"
    );
    if !clean_ok {
        assert!(
            clean_out.contains("temp residue"),
            "the guard failed on something other than residue before any orphan was planted:\n{clean_out}"
        );
        eprintln!(
            "NOTE: the clean probe reported pre-existing residue (not an orphan):\n{clean_out}"
        );
    }

    // --- planted orphan: the guard must fail and name it ----------------
    let orphan = plant_orphan(dir.path());
    let _reap = ReapOnDrop(orphan);

    let (ok, output) = run_guard(&[]);
    assert!(
        !ok,
        "the guard must exit non-zero when an orphaned server is running:\n{output}"
    );
    assert!(
        output.contains(&format!("orphaned dg serve: pid {orphan}")),
        "the guard must name the orphaned pid {orphan}:\n{output}"
    );
    // Reporting is the contract; killing is not.
    assert!(
        process_alive(orphan),
        "the guard killed a process it only had to report"
    );
}

/// A server whose parent is still alive is not an orphan, so a test suite can
/// run the guard without failing on its own server.
#[test]
fn a_server_owned_by_a_live_parent_is_not_reported_as_an_orphan() {
    let planted = plant_server();
    let (_ok, output) = run_guard(&[]);
    let needle = format!("pid {}", planted.pid);
    assert!(
        !output.contains(&needle),
        "the guard reported a server whose parent is alive (pid {}):\n{output}",
        planted.pid
    );
}
