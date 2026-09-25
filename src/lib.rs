//! Server foundation: loopback-only listener, authenticated API router,
//! and retention maintenance for tombstoned databases.

pub mod api;
pub mod backup;
pub mod client;
pub mod error;
pub mod execution;
pub mod shell;
pub mod store;
pub mod values;

use std::{fmt, net::SocketAddr};

use axum::{Json, Router, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;

#[derive(Debug)]
pub enum ServerError {
    NonLoopback,
    Io(std::io::Error),
    Application(error::Error),
}

impl ServerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NonLoopback => "invalid_listen_address",
            Self::Io(_) => "server_io_error",
            Self::Application(error) => error.code,
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonLoopback => write!(
                f,
                "only loopback listening is supported; reach remote hosts over an SSH tunnel or private network"
            ),
            Self::Io(error) => write!(f, "{error}"),
            Self::Application(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn validate_listen(listen: SocketAddr) -> Result<(), ServerError> {
    if !listen.ip().is_loopback() {
        return Err(ServerError::NonLoopback);
    }
    Ok(())
}

#[derive(Serialize)]
struct Liveness {
    status: &'static str,
}

/// Liveness is deliberately not database readiness or backup health.
pub fn router() -> Router {
    Router::new().route(
        "/health/live",
        get(|| async { Json(Liveness { status: "ok" }) }),
    )
}

pub async fn serve(
    listen: SocketAddr,
    data_dir: std::path::PathBuf,
    backup_root: Option<std::path::PathBuf>,
) -> Result<(), ServerError> {
    validate_listen(listen)?;
    let store = tokio::task::spawn_blocking(move || {
        store::Store::open(&data_dir).map(|s| match backup_root {
            // Flag present: explicit root (aside the convention default).
            Some(root) => s.with_backup_root(Some(root)),
            // Flag absent: `<data-dir>/../dg-backups` convention, unknown
            // when the directory is missing.
            None => s,
        })
    })
    .await
    .map_err(|_| ServerError::Application(error::Error::internal()))?
    .map_err(ServerError::Application)?;
    let store = std::sync::Arc::new(store);
    let app = router().merge(api::router(store.clone()));
    let listener = TcpListener::bind(listen).await?;
    tracing::info!(address = %listener.local_addr()?, "liveness server listening");
    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    tokio::select! {
        result = server.into_future() => result.map_err(ServerError::Io),
        _ = retention_maintenance(store) => unreachable!("maintenance runs until server shutdown"),
    }
}

/// Retention is enforced independently of incoming traffic, including at startup.
/// Busy requests defer purge to a later tick; failures are logged and retried.
async fn retention_maintenance(store: std::sync::Arc<store::Store>) {
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        let store = store.clone();
        match tokio::task::spawn_blocking(move || {
            store.purge_expired()?;
            store.sweep_transfers().map(|_| ())
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::error!(code = error.code, "retention purge failed; will retry")
            }
            Err(_) => tracing::error!(
                code = "purge_worker_failed",
                "retention purge worker failed; will retry"
            ),
        }
    }
}

async fn shutdown_signal() {
    // Register both signals before awaiting either. Log registration failure
    // and request shutdown rather than leaving a server without signal handling.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut interrupt), Ok(mut terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {},
                    _ = terminate.recv() => {},
                }
            }
            _ => tracing::error!(
                code = "signal_handler_error",
                "cannot register shutdown signals"
            ),
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(code = "signal_handler_error", message = %error, "cannot receive shutdown signal");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[test]
    fn only_loopback_addresses_are_allowed() {
        for address in ["127.0.0.1:7432", "[::1]:7432", "127.0.0.1:0"] {
            assert!(validate_listen(address.parse().unwrap()).is_ok());
        }
        for address in ["0.0.0.0:7432", "[::]:7432", "192.0.2.1:7432"] {
            let error = validate_listen(address.parse().unwrap()).unwrap_err();
            assert_eq!(error.code(), "invalid_listen_address");
        }
    }

    #[tokio::test]
    async fn liveness_is_json() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"status": "ok"})
        );
    }

    #[tokio::test]
    async fn database_api_is_not_exposed() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/v1/databases")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
