//! The rule for a destructive act: the operator repeats the exact name.
//!
//! One rule, read by the CLI's `--confirm` flag and by the terminal
//! view's prompt, so a name typed in one surface means exactly what
//! the same name means in the other. A destructive act that only one
//! surface protects is a destructive act the other surface will
//! perform on a stray keypress.

use crate::error::{Error, Result};

/// The error a refusal carries.
///
/// The message says what to do, not just what went wrong: the
/// operator is told how to give the consent the act needs.
pub fn required(message: &'static str) -> Error {
    Error::new(400, "confirmation_required", message)
}

/// Check that the operator repeated the exact name.
///
/// `confirm` is what the operator typed; `name` is the thing being
/// destroyed. They must match exactly — a prefix, a different case, or
/// an empty string is not consent.
pub fn confirm_matches(confirm: Option<&str>, name: &str) -> Result<()> {
    if confirm == Some(name) {
        return Ok(());
    }
    Err(required(
        "destructive operation: repeat the exact database name with --confirm NAME",
    ))
}

/// The irreversible-scope warning for a database-wide key revoke.
///
/// The confirmation carries the most important safety fact: this cuts
/// application credentials scoped to this database, and does NOT revoke
/// the owner key that the operator is using to issue the command.
pub fn confirm_database_revoke(confirm: Option<&str>, database: &str) -> Result<()> {
    if confirm == Some(database) {
        return Ok(());
    }
    Err(required(
        "bulk revoke removes application credentials scoped to this database; the owner key is never revoked; repeat the exact database name with --confirm NAME",
    ))
}
