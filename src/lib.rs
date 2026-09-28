//! Server foundation: loopback-only listener, authenticated API router,
//! and retention maintenance for tombstoned databases.

pub mod api;
pub mod backup;
pub mod backup_health;
pub mod client;
pub mod error;
pub mod execution;
pub mod notify;
pub mod shell;
pub mod store;
pub mod values;

use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

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

/// How backup-health notifications are delivered and when.
///
/// Typed, not stringly: a cadence that arrives as a parsed `u64` at the edge
/// and a `Duration` everywhere else cannot be silently wrong in the middle,
/// and the operator sees exactly one place where both numbers are named.
#[derive(Debug, Clone)]
pub struct BackupNotify {
    /// Sinks, in delivery order. The log sink is always present when
    /// delivery is enabled at all.
    pub sinks: Vec<Arc<dyn notify::Sink>>,
    /// How often the per-database digest goes out. Zero disables the digest
    /// and leaves only immediate failure notices, which is a legitimate
    /// configuration and not a broken one.
    pub digest_interval: Duration,
    /// How often the producer's attempt log is checked for a new failure.
    /// Zero disables the watch, and then failures surface only in the digest.
    pub failure_poll: Duration,
    /// The owner-approved snapshot cadence this gateway is measured against,
    /// reported in the digest so a reader knows the target it is judged by.
    pub cadence_secs: u64,
}

impl BackupNotify {
    /// Delivery off: no task, no sinks, no timers. The default, because a
    /// notification is a behaviour change an operator opts into.
    pub fn disabled() -> Self {
        Self {
            sinks: Vec::new(),
            digest_interval: Duration::ZERO,
            failure_poll: Duration::ZERO,
            cadence_secs: backup_health::STALE_AFTER_SECS,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.sinks.is_empty()
            && (self.digest_interval > Duration::ZERO || self.failure_poll > Duration::ZERO)
    }
}

pub async fn serve(
    listen: SocketAddr,
    data_dir: std::path::PathBuf,
    backup_root: Option<std::path::PathBuf>,
    backup_notify: BackupNotify,
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
    if backup_notify.enabled() {
        // Detached on purpose: the notifier runs until the process ends, and
        // the graceful-shutdown future below is what stops the server. A
        // shutdown race here would mean a lost final digest, not a crash.
        let _notifier = tokio::spawn(backup_notifier(store.clone(), backup_notify));
    }
    tokio::select! {
        result = server.into_future() => result.map_err(ServerError::Io),
        _ = retention_maintenance(store) => unreachable!("maintenance runs until server shutdown"),
    }
}

/// Backup-health delivery (observability contract §3 alert 4): an immediate
/// notice per failed attempt, and a per-database digest on a cadence.
///
/// Two independent timers in one task, because they answer different
/// questions and have different urgencies: a failure is news the moment it is
/// recorded, while "is `beta` still fresh" is a daily question. A single
/// combined tick would either poll for failures too slowly or send digests
/// far too often.
///
/// Nothing here can take the server down. Every failure -- an unreadable
/// registry, a snapshot that cannot be computed, a sink that refuses, even a
/// panic in one iteration -- is logged and the loop continues, because a
/// notification path that can crash a gateway has turned an alerting problem
/// into an availability one.
async fn backup_notifier(store: std::sync::Arc<store::Store>, config: BackupNotify) {
    let mut digest = Cadence::new(config.digest_interval);
    let mut failure = Cadence::new(config.failure_poll);
    // Delivery state starts empty on purpose: the first failure poll reports
    // the newest failed attempt even if it predates this process, so a
    // restart is never the reason a 03:00 backup failure goes unreported. It
    // reports that one attempt, and the daily digest carries the history.
    let mut delivered = notify::DeliveryState::default();
    loop {
        // Only enabled cadences vote. A disabled cadence contributes nothing
        // rather than a zero: taking `min` against a zero sentinel would let
        // an explicitly-disabled poll cancel a live digest, and the notifier
        // would go quiet while still logged as enabled.
        let wait = match (digest.until_next(), failure.until_next()) {
            (Some(digest), Some(failure)) => Some(digest.min(failure)),
            (Some(digest), None) => Some(digest),
            (None, Some(failure)) => Some(failure),
            (None, None) => None,
        };
        // Nothing scheduled: delivery is on (a sink is configured) but no
        // cadence asks for anything. Returning ends the task rather than
        // spinning on a core forever.
        let Some(wait) = wait else { return };
        tokio::time::sleep(wait).await;
        if digest.due() {
            match backup_health::snapshot(&store) {
                Ok(health) => {
                    let note = notify::digest_note(&health);
                    if !notify::deliver_all(&config.sinks, &note).await.is_empty() {
                        tracing::error!(
                            code = "backup_digest_delivery_failed",
                            "backup digest delivery failed"
                        );
                    }
                }
                Err(error) => tracing::error!(
                    code = "backup_digest_failed",
                    reason = %error,
                    "cannot compute backup health; digest not sent"
                ),
            }
        }
        if failure.due() {
            if let Some(attempt) = last_failed_attempt(&store) {
                if delivered.should_report(&attempt) {
                    if let Ok(health) = backup_health::snapshot(&store) {
                        let note = notify::failure_note(&health, &attempt);
                        if !notify::deliver_all(&config.sinks, &note).await.is_empty() {
                            tracing::error!(
                                code = "backup_failure_delivery_failed",
                                attempt = %attempt.ts,
                                "backup failure notice delivery failed"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The newest failed attempt the producer recorded, if any.
fn last_failed_attempt(store: &store::Store) -> Option<backup_health::AttemptRecord> {
    let root = store.backup_root()?;
    backup_health::read_attempts(&root)
        .into_iter()
        .filter(|attempt| attempt.failed())
        .next_back()
}

/// One timer, built so the FIRST tick is a full period away.
///
/// `tokio::time::interval` fires immediately, which for a daily digest means
/// every restart sends one -- including the dozen restarts of a deploy, each
/// of which would look like a separate digest. A cadence is a schedule, and a
/// schedule that starts now is not one.
#[derive(Debug)]
struct Cadence {
    period: Duration,
    next: tokio::time::Instant,
}

impl Cadence {
    fn new(period: Duration) -> Self {
        Self {
            period,
            next: tokio::time::Instant::now() + period,
        }
    }

    fn disabled(&self) -> bool {
        self.period.is_zero()
    }

    /// Time until the next tick, or `None` when this cadence is off.
    /// Saturating rather than subtracting raw, so a tick that came due while
    /// a sink was slow reads as "due now" instead of panicking.
    fn until_next(&self) -> Option<Duration> {
        if self.disabled() {
            return None;
        }
        Some(
            self.next
                .saturating_duration_since(tokio::time::Instant::now()),
        )
    }

    /// True once the period has elapsed, rescheduling from now. A tick that
    /// arrives late runs once, not once per missed period: a backup digest
    /// is a statement about the moment it was computed, and replaying
    /// yesterday's digest after an outage would be a lie.
    fn due(&mut self) -> bool {
        if self.disabled() || tokio::time::Instant::now() < self.next {
            return false;
        }
        self.next = tokio::time::Instant::now() + self.period;
        true
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
