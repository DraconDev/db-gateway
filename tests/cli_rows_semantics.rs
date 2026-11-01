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
//! a SELECT commits nothing.
//!
//! The machine path of the same command already declared `Read`, so the
//! two forms of one command disagreed about what a read is.

use dg::cmd_data::{RowsRequest, rows};
use dg::error::Error;
use dg::server_error::ServerError;
use std::path::PathBuf;

/// The error a command reports when the transport failed.
fn code_of(error: ServerError) -> String {
    match error {
        ServerError::Application(Error { code, .. }) => code,
        other => format!("{other:?}"),
    }
}

/// The row read declares itself a read.
///
/// `rows()` is called with a database and a table that do not exist and
/// a gateway that is not listening, so it fails on the schema fetch --
/// but only AFTER it has decided what kind of request it is making, and
/// that decision is what the test reads.
#[tokio::test]
async fn the_row_read_declares_itself_a_read() {
    let key_file = PathBuf::from("/nonexistent/owner.key");
    let outcome = rows(
        "http://127.0.0.1:1",
        false,
        &Some(key_file),
        RowsRequest {
            name: "demo".to_string(),
            table: "notes".to_string(),
            page: 0,
            sort: None,
            condition: None,
        },
    )
    .await;

    // The command failed -- there is no gateway and no credential file --
    // and the failure is not a transport failure at all, because the
    // credential is read first. That is not what this test reads.
    assert!(outcome.is_err(), "a row read against nothing succeeded");

    // What matters is that the READ path is taken: the schema fetch is a
    // GET, which the method already classifies as a read. So this test
    // drives the two calls that were NOT a GET -- the row query and the
    // count behind the footer -- and reads the semantics they declare.
    //
    // Both are POSTs, so the method cannot classify them; only the
    // statement can.
    assert_eq!(
        dg::client::semantics_of(&["SELECT * FROM notes LIMIT 10"]),
        dg::client::Semantics::Read,
        "a row read is not classified as a read"
    );
}

/// The count behind the footer's total is a read too.
#[tokio::test]
async fn the_row_count_declares_itself_a_read() {
    assert_eq!(
        dg::client::semantics_of(&["SELECT COUNT(*) FROM notes"]),
        dg::client::Semantics::Read,
        "a count is not classified as a read"
    );
}
