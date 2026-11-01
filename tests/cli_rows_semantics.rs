//! `dg rows` is a read, and the transport has to be told so.
//!
//! The CLI classifies a request by its semantics, not by its HTTP method:
//! a query is a POST either way, so the SQL decides. That is what makes
//! a read give up after ten seconds and report an unavailable gateway,
//! while a write gives up after sixty and reports `outcome_unknown` --
//! "writes may have committed, replay only with the same idempotency
//! key".
//!
//! `dg rows` had two calls that did not declare their semantics, so the
//! client fell back to the method and classified a SELECT as a write: it
//! waited the write timeout, and on failure it told the operator their
//! read might have committed -- advice that is unfollowable, because the
//! command never sends an idempotency key and a SELECT commits nothing.
//!
//! These tests drive the real binary against a gateway that is not
//! listening, so the transport failure is the observable.

use std::process::Command;

/// Run `dg rows` against an origin nothing is listening on.
fn rows_against_a_dead_gateway(database: &str) -> (bool, String, String) {
    // The credential has to be a real, readable key file: the CLI reads
    // it before it opens the connection, and a missing key would be
    // refused before the transport was ever tried.
    let key_file = std::env::temp_dir().join(format!("dg-rows-sem-{}", std::process::id()));
    std::fs::write(&key_file, "owner-key-for-the-test").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            // Port 1 is reserved and nothing listens on it.
            "http://127.0.0.1:1",
            "--key-file",
            key_file.to_str().unwrap(),
            "rows",
            database,
            "notes",
        ])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&key_file);
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A row read that cannot reach the gateway is an unavailable gateway.
///
/// It must NOT be reported as `outcome_unknown`: nothing was written, so
/// there is no outcome in doubt, and the retry advice that comes with it
/// ("replay only with the same idempotency key within its retention
/// window") points at a key this command never sends.
#[test]
fn a_row_read_against_a_dead_gateway_is_not_reported_as_a_write() {
    let (success, stdout, stderr) = rows_against_a_dead_gateway("demo");
    let said = format!("{stdout}{stderr}");
    assert!(
        !success,
        "a row read against a dead gateway reported success: {said}"
    );
    assert!(
        !said.contains("outcome_unknown"),
        "a row read is reported as a write that may have committed: {said}"
    );
    assert!(
        !said.contains("may have committed"),
        "a row read is reported as a write that may have committed: {said}"
    );
    assert!(
        said.contains("transport_unavailable"),
        "a row read against a dead gateway is not an unavailable gateway: {said}"
    );
}
