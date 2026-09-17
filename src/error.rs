use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: &'static str,
    #[serde(skip)]
    pub status: u16,
}
impl Error {
    pub fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }
    pub fn invalid() -> Self {
        Self::new(400, "invalid_request", "invalid request")
    }
    pub fn internal() -> Self {
        Self::new(500, "internal_error", "internal operation failed")
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
