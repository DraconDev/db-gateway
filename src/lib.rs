//! Protocol-neutral foundation. Database execution and credentials are not implemented.

pub mod client;
pub mod api;
pub mod store;
pub mod error;
pub mod execution;
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
                "only loopback listening is supported before authentication is implemented"
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

pub async fn serve(listen: SocketAddr, data_dir: std::path::PathBuf) -> Result<(), ServerError> {
    validate_listen(listen)?;
    let store = tokio::task::spawn_blocking(move || store::Store::open(&data_dir)).await.map_err(|_| ServerError::Application(error::Error::internal()))?.map_err(ServerError::Application)?;
    let app = router().merge(api::router(std::sync::Arc::new(store)));
    let listener = TcpListener::bind(listen).await?;
    tracing::info!(address = %listener.local_addr()?, "liveness server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
