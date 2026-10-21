use std::borrow::Cow;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    /// The operator-facing text.
    ///
    /// Most errors carry a fixed sentence, but some carry a number the
    /// operator has to read — how many statements a file holds, for
    /// instance. The text is therefore borrowed when it is fixed and
    /// owned when it is computed, so a computed message does not have
    /// to be smuggled through a `&'static str`.
    pub message: Cow<'static, str>,
    #[serde(skip)]
    pub status: u16,
    /// The statement of a batch that failed, when the failure belongs
    /// to one. A batch runs as a whole, so without this the operator
    /// cannot tell WHICH statement broke the script.
    ///
    /// It is omitted when there is none, so every existing error
    /// envelope stays byte for byte what it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<u32>,
}
impl Error {
    pub fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message: Cow::Borrowed(message),
            statement: None,
        }
    }
    /// An error whose message is computed.
    ///
    /// The number in the message is the point of the error: an operator
    /// who is told "the file holds 40 statements, the batch API accepts
    /// 32" knows exactly what to do, and a message that hid the count
    /// behind a fixed sentence would not.
    pub fn with_message(status: u16, code: &'static str, message: String) -> Self {
        Self {
            status,
            code,
            message: Cow::Owned(message),
            statement: None,
        }
    }
    /// The same error, attributed to a statement of a batch.
    pub fn at_statement(mut self, index: u32) -> Self {
        self.statement = Some(index);
        self
    }
    pub fn invalid() -> Self {
        Self::new(400, "invalid_request", "invalid request")
    }
    pub fn internal() -> Self {
        Self::new(500, "internal_error", "internal operation failed")
    }
    pub fn metadata() -> Self {
        Self::new(
            500,
            "metadata_corrupt",
            "metadata registry unreadable; see server log for the failing row",
        )
    }
    pub fn busy() -> Self {
        Self::new(
            429,
            "capacity_exceeded",
            "bounded capacity exhausted; retry later",
        )
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({"error": self})),
        )
            .into_response()
    }
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode::*;
        match error.sqlite_error_code() {
            Some(AuthorizationForStatementDenied | PermissionDenied | ReadOnly) => {
                Self::new(403, "sql_forbidden", "SQL operation not permitted")
            }
            Some(OperationInterrupted) => Self::new(
                408,
                "query_deadline_exceeded",
                "execution deadline exceeded; transaction rolled back",
            ),
            Some(DatabaseBusy | DatabaseLocked) => Self::busy(),
            Some(DiskFull | SystemIoFailure | DatabaseCorrupt) => Self::internal(),
            _ => Self::new(400, "invalid_sql", "SQL or parameters invalid"),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A full disk surfaces as SQLITE_FULL from SQLite; the gateway must
    /// answer with the stable storage error, not a panic or a dropped
    /// connection. (The integration half — writer death under growth
    /// pressure plus restart recovery — lives in tests/disk_pressure.rs.)
    #[test]
    fn sqlite_full_maps_to_stable_internal_error() {
        let full = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(13), None);
        assert_eq!(
            full.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DiskFull)
        );
        let mapped = Error::from(full);
        assert_eq!(mapped.status, 500);
        assert_eq!(mapped.code, "internal_error");
    }
}
