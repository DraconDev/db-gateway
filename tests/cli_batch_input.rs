//! CLI batch-input safety: non-regular files and the byte boundary are
//! rejected locally and promptly, before credentials or the network are used.

#![cfg(unix)]

use std::{
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn run_batch(path: &Path) -> (bool, String, String) {
    let home = tempfile::tempdir().expect("empty HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args(["batch", "input_db"])
        .arg(path)
        .env("HOME", home.path())
        .env_remove("DG_KEY_FILE")
        .env_remove("DG_SERVER")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dg batch");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait().expect("poll dg batch") {
            Some(_) => break,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("collect timed-out dg batch");
                panic!(
                    "dg batch blocked on input for more than two seconds: stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
        }
    }

    let output = child.wait_with_output().expect("collect dg batch output");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn batch_rejects_writerless_fifo_promptly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fifo = dir.path().join("batch.fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
    let rc = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

    let (success, stdout, stderr) = run_batch(&fifo);
    assert!(
        !success,
        "writer-less FIFO unexpectedly succeeded: {stdout}"
    );
    assert!(
        stderr.contains("invalid_request"),
        "unexpected error: {stderr}"
    );
    assert!(stdout.is_empty(), "FIFO rejection wrote stdout: {stdout}");
}

#[test]
fn batch_enforces_the_64_kib_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let valid_batch = br#"{"atomic":true,"statements":[]}"#;

    let at_limit = dir.path().join("at-limit.json");
    let mut at_limit_bytes = valid_batch.to_vec();
    at_limit_bytes.resize(65_536, b' ');
    fs::write(&at_limit, at_limit_bytes).expect("write at-limit input");
    let (success, stdout, stderr) = run_batch(&at_limit);
    assert!(!success, "at-limit input unexpectedly succeeded");
    assert!(
        stderr.contains("missing_credential"),
        "exactly 64 KiB was not parsed and passed to credential handling: {stderr}"
    );
    assert!(
        !stderr.contains("input_limit_exceeded"),
        "exactly 64 KiB must not trip the size limit: {stderr}"
    );
    assert!(stdout.is_empty(), "at-limit input wrote stdout: {stdout}");

    let over_limit = dir.path().join("over-limit.json");
    let mut over_limit_bytes = valid_batch.to_vec();
    over_limit_bytes.resize(65_537, b' ');
    fs::write(&over_limit, over_limit_bytes).expect("write over-limit input");
    let (success, _, stderr) = run_batch(&over_limit);
    assert!(!success, "oversized input unexpectedly succeeded");
    assert!(
        stderr.contains("input_limit_exceeded"),
        "oversized input was rejected for the wrong reason: {stderr}"
    );
}
