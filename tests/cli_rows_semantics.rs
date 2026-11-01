//! `dg rows` is a read, and the transport has to be told so.
//!
//! The CLI classifies a request by its semantics, not by its HTTP method:
//! a query is a POST either way, so the SQL decides. That is what makes
//! a read give up after ten seconds and report an unavailable gateway,
//! while a write gives up after sixty and reports `outcome_unknown` --
//! "writes may have committed, replay only with the same idempotency
//! key".
//!
//! The human path of `dg rows` had two calls that did not declare their
//! semantics, so the client fell back to the method and classified a
//! SELECT as a write: it waited the write timeout, and on failure it
//! told the operator their read might have committed -- advice that is
//! unfollowable, because the command never sends an idempotency key and
//! a SELECT commits nothing. The machine path of the same command
//! already declared `Read`, so the two forms of one command disagreed
//! about what a read is.
//!
//! The test drives the real binary on a real terminal (the human path is
//! only taken when stdout is a terminal), against a gateway that is not
//! listening, so the transport failure is the observable.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

/// Draw `dg rows` on a real terminal, against a gateway that is not
/// there, and read what an operator reads.
fn rows_on_a_terminal() -> String {
    let key_file = std::env::temp_dir().join(format!("dg-rows-tty-{}", std::process::id()));
    // The credential has to LOOK like a credential: the CLI refuses
    // anything that is not 64 hex characters before it opens a
    // connection, and a refused key would hide the transport failure
    // this test is about.
    std::fs::write(&key_file, "a".repeat(64)).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();

    // `script` gives the child a terminal on stdout, so the command
    // takes its human path.
    let command = format!(
        "{} --server http://127.0.0.1:1 --key-file {} rows demo notes",
        env!("CARGO_BIN_EXE_dg"),
        key_file.to_str().unwrap()
    );
    let output = Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&key_file);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A row read that cannot reach the gateway is an unavailable gateway.
///
/// It must NOT be reported as `outcome_unknown`: nothing was written, so
/// there is no outcome in doubt, and the retry advice that comes with it
/// ("replay only with the same idempotency key within its retention
/// window") points at a key this command never sends.
#[test]
fn a_row_read_against_a_dead_gateway_is_not_reported_as_a_write() {
    let text = rows_on_a_terminal();
    assert!(
        !text.contains("outcome_unknown"),
        "a row read is reported as a write that may have committed: {text}"
    );
    assert!(
        !text.contains("may have committed"),
        "a row read is reported as a write that may have committed: {text}"
    );
    assert!(
        text.contains("transport_unavailable"),
        "a row read against a dead gateway is not an unavailable gateway: {text}"
    );
}

/// The key file the test used, kept so a failure can be reproduced.
#[allow(dead_code)]
fn key_path() -> PathBuf {
    std::env::temp_dir().join(format!("dg-rows-tty-{}", std::process::id()))
}
