//! Backup-health notification delivery (observability contract §3 alert 4).
//!
//! The exposure half shipped first: `GET /v1/backups` and `dg backup status`
//! answer "how fresh is each database" when someone asks. The owner's
//! 2026-09-27 decision (docs/recovery-decisions.md §6.5) is about the other
//! half: **immediate notice on failure plus a daily digest** of per-database
//! fresh/stale/unknown. Nobody polls an endpoint at 03:00, so an answer that
//! only exists on request is not delivery.
//!
//! ## The floor, and why these two sinks
//!
//! No alert stack exists in this project today, so this module ships the
//! smallest thing that is genuinely delivery rather than a printed report:
//!
//! * [`LogSink`] is always on. Structured tracing is already the service's
//!   operator channel (systemd captures stderr), so with no configuration at
//!   all every digest and every failure notice reaches the operator's log
//!   pipeline. This is the floor the decision names, and it is also the sink
//!   that cannot silently stop working: a webhook that 500s is a delivered
//!   notice; a digest that only exists in memory is not.
//! * [`WebhookSink`] is opt-in, configured by an explicit flag. It is the
//!   first step toward a real alert stack, and it is deliberately not wired
//!   to an environment variable: a notification that starts leaving the host
//!   because an env var appeared in a unit file is not a change anyone
//!   reviewed.
//!
//! ## What a notification is allowed to claim
//!
//! Contract §3: identify the database, the last verified timestamp, the
//! current age, the failure reason, and a bounded operator action -- and claim
//! no data-loss prevention, RPO or RTO, because none is accepted yet. The
//! types here make that structural rather than a writing discipline: a [`Note`]
//! has no field for an RPO, so there is nowhere to put one.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::backup_health::{AttemptRecord, BackupHealth};

/// What a notification is about. Serialized as the `kind` field, so a sink
/// can route without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// A backup attempt failed. Immediate, once per attempt.
    BackupFailure,
    /// The scheduled per-database digest.
    BackupDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Something is wrong: a failed attempt, or a database that is not fresh.
    Alert,
    /// A periodic summary, including an all-fresh one. Reporting "all
    /// databases fresh" daily is the point: silence is indistinguishable from
    /// a broken notifier.
    Digest,
}

/// One delivered notification. Everything contract §3 requires a backup
/// alert to carry, and nothing it forbids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub kind: NoteKind,
    pub severity: Severity,
    /// Operator-facing subject line.
    pub title: String,
    /// Per-database state, carried even when empty so a sink can tell "all
    /// fresh" from "nothing to report".
    pub databases: Vec<crate::backup_health::DatabaseHealth>,
    /// The newest attempt the producer recorded, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<AttemptRecord>,
    /// Set on a failure note: the attempt that failed, and the step it
    /// reached. This is the failure reason, and it is deliberately the
    /// producer's own record rather than a guess from a missing set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<AttemptRecord>,
    /// The bounded operator action. Never an instruction to trust the
    /// backup, and never an RPO.
    pub action: String,
    /// The §2 look-at-it reference the states are judged against, named as
    /// `/v1/backups` names it so the two read the same. Deliberately not
    /// called an RPO: it is an objective nobody has accepted as a guarantee,
    /// and a notification that carried one would be making a promise the
    /// project has not earned.
    pub stale_after_secs: u64,
}

/// Render a failure notice for one attempt. The database list is carried
/// because "the attempt failed" and "which databases are affected" are the
/// two things an operator needs together at 03:00.
pub fn failure_note(health: &BackupHealth, attempt: &AttemptRecord) -> Note {
    let step = if attempt.step.is_empty() {
        "unknown step".to_string()
    } else {
        format!("step {}", attempt.step)
    };
    let databases = if attempt.databases > 0 {
        format!(" after capturing {} database(s)", attempt.databases)
    } else {
        String::new()
    };
    Note {
        kind: NoteKind::BackupFailure,
        severity: Severity::Alert,
        title: format!(
            "backup attempt {} failed at {step}{databases}",
            if attempt.ts.is_empty() {
                "?"
            } else {
                &attempt.ts
            }
        ),
        databases: health.databases.clone(),
        last_attempt: health.last_attempt.clone(),
        failure: Some(attempt.clone()),
        action: format!(
            "run `dg backup verify --backup-root {} latest`, then read \
             $BACKUP_ROOT/attempts.jsonl for the failing step",
            health
                .backup_root
                .as_ref()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "<root>".to_string())
        ),
        stale_after_secs: health.stale_after_secs,
    }
}

/// Render the daily digest: every database, its state, its newest verified
/// set and its age, plus the counts an operator reads first.
pub fn digest_note(health: &BackupHealth) -> Note {
    let (fresh, stale, unknown) = health.counts();
    let attention = health.attention().len();
    let title = if attention == 0 {
        format!("backup digest: {fresh} database(s) fresh")
    } else {
        format!("backup digest: {fresh} fresh, {stale} stale, {unknown} unknown")
    };
    let action = if attention == 0 {
        "no action; re-verify a set on demand with `dg backup verify`".to_string()
    } else {
        format!(
            "run `dg backup status` for the per-database ages, then \
             `dg backup verify --backup-root {} latest`",
            health
                .backup_root
                .as_ref()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "<root>".to_string())
        )
    };
    Note {
        kind: NoteKind::BackupDigest,
        severity: if attention == 0 {
            Severity::Digest
        } else {
            Severity::Alert
        },
        title,
        databases: health.databases.clone(),
        last_attempt: health.last_attempt.clone(),
        failure: health.last_failure.clone(),
        action,
        stale_after_secs: health.stale_after_secs,
    }
}

/// The single-line form an operator reads in a log tail, a chat sink or a
/// mail subject. Kept one line per database so a long list is still
/// greppable, and so a sink that truncates shows whole rows.
pub fn render_text(note: &Note) -> String {
    let mut out = format!("[{}] {}", note.kind.as_str(), note.title);
    for db in &note.databases {
        out.push_str(&format!("\n  {} {}", db.state, db.name));
        if let Some(set) = &db.newest_set {
            out.push_str(&format!(" (newest {set}"));
            if let Some(age) = db.age_secs {
                out.push_str(&format!(", {age}s ago"));
            }
            out.push(')');
        }
    }
    if let Some(failure) = &note.failure {
        out.push_str(&format!(
            "\n  failed attempt {} at step {}",
            if failure.ts.is_empty() {
                "?"
            } else {
                &failure.ts
            },
            if failure.step.is_empty() {
                "?"
            } else {
                &failure.step
            }
        ));
    }
    out.push_str(&format!(
        "\n  judged against a {}s reference; action: {}",
        note.stale_after_secs, note.action
    ));
    out
}

impl NoteKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BackupFailure => "backup_failure",
            Self::BackupDigest => "backup_digest",
        }
    }
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::Digest => "digest",
        }
    }
}

/// Where a notification goes.
///
/// Async and dyn-safe, because the only HTTP client this project trusts is
/// the async `reqwest` one the CLI already uses (same no-proxy,
/// no-redirect, bounded-timeout transport discipline), and a sink trait that
/// forced a second, weaker HTTP stack into the binary would be a worse
/// trade than one boxed future. The notifier runs in its own task, so a sink
/// that takes seconds to answer delays the next digest, never a request.
pub trait Sink: Send + Sync + std::fmt::Debug {
    /// A short identifier for logs, e.g. `log` or `webhook`.
    fn name(&self) -> &'static str;
    fn deliver<'a>(
        &'a self,
        note: &'a Note,
    ) -> Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>>;
}

#[derive(Debug)]
pub struct SinkError {
    pub sink: &'static str,
    pub reason: String,
}

impl SinkError {
    fn new(sink: &'static str, reason: impl Into<String>) -> Self {
        Self {
            sink,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} sink: {}", self.sink, self.reason)
    }
}

impl std::error::Error for SinkError {}

/// The always-on sink: one structured line per notification.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogSink;

impl Sink for LogSink {
    fn name(&self) -> &'static str {
        "log"
    }

    fn deliver<'a>(
        &'a self,
        note: &'a Note,
    ) -> Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>> {
        Box::pin(async move {
            let databases = note
                .databases
                .iter()
                .map(|db| db.name.as_str())
                .collect::<Vec<_>>()
                .join(",");
            // Fields, not prose, so an operator's log pipeline can alert on
            // `kind=backup_failure` without parsing English. The rendered text
            // rides along because most operators read logs, not JSON.
            if note.kind == NoteKind::BackupFailure {
                tracing::error!(
                    kind = note.kind.as_str(),
                    severity = note.severity.as_str(),
                    databases,
                    attempt = note
                        .failure
                        .as_ref()
                        .map(|f| f.ts.as_str())
                        .unwrap_or_default(),
                    step = note
                        .failure
                        .as_ref()
                        .map(|f| f.step.as_str())
                        .unwrap_or_default(),
                    action = %note.action,
                    summary = %render_text(note),
                    "backup attempt failed"
                );
            } else {
                let (fresh, stale, unknown) = (
                    note.databases.iter().filter(|d| d.state == "fresh").count(),
                    note.databases.iter().filter(|d| d.state == "stale").count(),
                    note.databases
                        .iter()
                        .filter(|d| d.state == "unknown")
                        .count(),
                );
                tracing::info!(
                    kind = note.kind.as_str(),
                    severity = note.severity.as_str(),
                    databases,
                    fresh,
                    stale,
                    unknown,
                    action = %note.action,
                    summary = %render_text(note),
                    "backup digest"
                );
            }
            Ok(())
        })
    }
}

/// Opt-in HTTP delivery. Posts the note as JSON and treats a non-2xx as a
/// delivery failure that is reported, not swallowed: a webhook that has been
/// rejecting for a week must be visible, because the alternative is an
/// operator believing they are covered by a sink that stopped working.
#[derive(Debug, Clone)]
pub struct WebhookSink {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
}

impl WebhookSink {
    /// Build a webhook sink for an owner-configured URL.
    ///
    /// The same transport rule the CLI applies to `--server`: HTTPS, or
    /// plain HTTP on a literal loopback address. A digest carries database
    /// names and backup state, so allowing it onto the network in cleartext
    /// would be a leak the flag should never be able to cause; loopback is
    /// exempt because a local collector is the realistic first deployment.
    /// No ambient proxy, no redirects: a notification that silently detoured
    /// through a proxy or a redirect would be delivered somewhere nobody
    /// reviewed.
    pub fn new(url: impl Into<String>) -> Result<Self, SinkError> {
        Self::with_timeout(url, Duration::from_secs(10))
    }

    pub fn with_timeout(url: impl Into<String>, timeout: Duration) -> Result<Self, SinkError> {
        let url = url.into();
        let parsed = reqwest::Url::parse(&url)
            .map_err(|error| SinkError::new("webhook", format!("unparseable url: {error}")))?;
        let loopback = parsed
            .host_str()
            .map(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !(parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback)) {
            return Err(SinkError::new(
                "webhook",
                "use an https url, or plain http on a literal loopback address",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|error| SinkError::new("webhook", error.to_string()))?;
        Ok(Self {
            client,
            url,
            timeout,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Sink for WebhookSink {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn deliver<'a>(
        &'a self,
        note: &'a Note,
    ) -> Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>> {
        Box::pin(async move {
            let response = self
                .client
                .post(&self.url)
                .json(note)
                .send()
                .await
                .map_err(|error| SinkError::new("webhook", error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                return Err(SinkError::new("webhook", format!("status {status}")));
            }
            Ok(())
        })
    }
}

/// Deliver to every sink, reporting each failure without letting one sink
/// starve the others: a webhook outage must not cost the operator the log
/// line, which is the sink that always works.
pub async fn deliver_all(sinks: &[Arc<dyn Sink>], note: &Note) -> Vec<SinkError> {
    let mut errors = Vec::new();
    for sink in sinks {
        if let Err(error) = sink.deliver(note).await {
            tracing::error!(
                code = "notification_sink_failed",
                sink = error.sink,
                kind = note.kind.as_str(),
                reason = %error.reason,
                "notification sink failed; other sinks still delivered"
            );
            errors.push(error);
        }
    }
    errors
}

/// Delivery state that must survive across polls: an immediate notice fires
/// once per failed attempt, not once per poll. Without this a 60s poll
/// against a persistently failing backup would turn one failure into 1,440
/// identical alerts a day, which is how operators learn to ignore alerts.
#[derive(Debug, Default)]
pub struct DeliveryState {
    /// Identity of the last attempt reported as failed. `None` means
    /// nothing has been reported yet, so the first failure is reported.
    reported_failure: Option<String>,
}

impl DeliveryState {
    /// Decide whether a failure notice is due, and remember it if so.
    ///
    /// Only a FAILING attempt is ever remembered, so a later success does
    /// not silence the next distinct failure.
    pub fn should_report(&mut self, attempt: &AttemptRecord) -> bool {
        if !attempt.failed() {
            return false;
        }
        let identity = attempt.identity().to_string();
        if self.reported_failure.as_deref() == Some(identity.as_str()) {
            return false;
        }
        self.reported_failure = Some(identity);
        true
    }

    /// The failure identity already reported, for tests and for a restart
    /// that wants to avoid re-announcing.
    pub fn reported(&self) -> Option<&str> {
        self.reported_failure.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup_health::DatabaseHealth;
    use std::sync::Mutex;

    fn health() -> BackupHealth {
        BackupHealth {
            backup_root: Some(std::path::PathBuf::from("/var/lib/dg-backups")),
            stale_after_secs: 300,
            generated_at: 1_700_000_000,
            databases: vec![
                DatabaseHealth {
                    name: "alpha".into(),
                    state: "fresh",
                    newest_set: Some("20240101T000000Z".into()),
                    age_secs: Some(60),
                },
                DatabaseHealth {
                    name: "beta".into(),
                    state: "stale",
                    newest_set: Some("20240101T000000Z".into()),
                    age_secs: Some(9_000),
                },
                DatabaseHealth {
                    name: "gamma".into(),
                    state: "unknown",
                    newest_set: None,
                    age_secs: None,
                },
            ],
            last_attempt: None,
            last_failure: None,
        }
    }

    fn attempt(ts: &str, outcome: &str, step: &str) -> AttemptRecord {
        AttemptRecord {
            ts: ts.into(),
            started_at: 1,
            completed_at: 2,
            outcome: outcome.into(),
            set: None,
            databases: 0,
            step: step.into(),
        }
    }

    #[derive(Debug, Default)]
    struct Recorder {
        notes: Mutex<Vec<Note>>,
    }

    impl Recorder {
        fn notes(&self) -> Vec<Note> {
            self.notes.lock().unwrap().clone()
        }
    }

    impl Sink for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }

        fn deliver<'a>(
            &'a self,
            note: &'a Note,
        ) -> Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>> {
            Box::pin(async move {
                self.notes.lock().unwrap().push(note.clone());
                Ok(())
            })
        }
    }

    #[derive(Debug)]
    struct Failing;

    impl Sink for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn deliver<'a>(
            &'a self,
            _note: &'a Note,
        ) -> Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>> {
            Box::pin(async { Err(SinkError::new("failing", "deliberate")) })
        }
    }

    #[test]
    fn digest_carries_every_database_its_state_and_its_age() {
        let note = digest_note(&health());
        assert_eq!(note.kind, NoteKind::BackupDigest);
        assert_eq!(
            note.severity,
            Severity::Alert,
            "a stale database is an alert"
        );
        assert_eq!(note.databases.len(), 3);
        assert!(note.title.contains("1 fresh"));
        assert!(note.title.contains("1 stale"));
        assert!(note.title.contains("1 unknown"));
        let text = render_text(&note);
        for needle in [
            "fresh alpha",
            "stale beta",
            "unknown gamma",
            "60s ago",
            "9000s ago",
        ] {
            assert!(text.contains(needle), "{needle} missing from:\n{text}");
        }
        // Contract section 3: a bounded operator action, every time.
        assert!(note.action.contains("dg backup status"), "{}", note.action);
    }

    #[test]
    fn an_all_fresh_digest_is_still_a_digest_and_still_has_an_action() {
        let mut health = health();
        for db in &mut health.databases {
            db.state = "fresh";
        }
        let note = digest_note(&health);
        assert_eq!(note.severity, Severity::Digest);
        assert!(note.title.contains("3 database(s) fresh"));
        assert!(!note.action.is_empty());
    }

    #[test]
    fn a_failure_note_names_the_attempt_the_step_and_the_action() {
        let note = failure_note(&health(), &attempt("20240102T000000Z", "error", "manifest"));
        assert_eq!(note.kind, NoteKind::BackupFailure);
        assert_eq!(note.severity, Severity::Alert);
        // The exact title, not a substring: an earlier build read
        // "failedfailed at step" and a `contains` assertion was happy with
        // it.
        assert_eq!(
            note.title,
            "backup attempt 20240102T000000Z failed at step manifest",
        );
        // No step recorded is still a sentence an operator can act on.
        let bare = failure_note(&health(), &attempt("t", "error", ""));
        assert_eq!(bare.title, "backup attempt t failed at unknown step");
        let text = render_text(&note);
        assert!(
            text.contains("failed attempt 20240102T000000Z at step manifest"),
            "{text}"
        );
        assert!(text.contains("dg backup verify"), "{text}");
        // The failure record travels with the note, so a sink can route on
        // it instead of parsing the title.
        assert_eq!(note.failure.as_ref().unwrap().step, "manifest");
    }

    #[test]
    fn a_note_names_the_reference_it_is_judged_against() {
        let note = digest_note(&health());
        assert_eq!(note.stale_after_secs, 300);
        assert!(render_text(&note).contains("300s reference"));
    }

    #[test]
    fn a_note_has_no_field_that_could_claim_an_rpo() {
        let note = digest_note(&health());
        let json = serde_json::to_string(&note).unwrap();
        for forbidden in ["rpo", "rto", "no_data_loss", "recovered"] {
            assert!(
                !json.to_lowercase().contains(forbidden),
                "notification claims {forbidden}: {json}"
            );
        }
    }

    #[tokio::test]
    async fn one_sink_failing_never_starves_the_others() {
        let recorder = Arc::new(Recorder::default());
        let handle = recorder.clone();
        let sinks: Vec<Arc<dyn Sink>> = vec![Arc::new(Failing), recorder];
        let errors = deliver_all(&sinks, &digest_note(&health())).await;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].sink, "failing");
        assert_eq!(handle.notes().len(), 1, "the healthy sink still delivered");
    }

    #[test]
    fn a_failure_is_reported_once_and_a_later_distinct_one_again() {
        let mut state = DeliveryState::default();
        let first = attempt("a", "error", "export");
        let again = attempt("a", "error", "export");
        let second = attempt("b", "error", "publish");
        assert!(state.should_report(&first));
        assert!(
            !state.should_report(&again),
            "the same attempt must not re-alert"
        );
        assert!(state.should_report(&second), "a new failure must alert");
        assert_eq!(state.reported(), Some("b"));
    }

    #[test]
    fn a_success_never_reports_and_never_silences_the_next_failure() {
        let mut state = DeliveryState::default();
        assert!(!state.should_report(&attempt("a", "ok", "publish")));
        assert!(state.should_report(&attempt("b", "error", "export")));
    }

    #[test]
    fn an_attempt_without_a_timestamp_still_deduplicates() {
        let mut state = DeliveryState::default();
        let anonymous = attempt("", "error", "init");
        assert!(state.should_report(&anonymous));
        assert!(!state.should_report(&attempt("", "error", "init")));
    }

    #[tokio::test]
    async fn the_log_sink_always_accepts() {
        let note = digest_note(&health());
        assert!(LogSink.deliver(&note).await.is_ok());
        assert!(
            LogSink
                .deliver(&failure_note(&health(), &attempt("a", "error", "x")))
                .await
                .is_ok()
        );
    }

    #[test]
    fn a_webhook_sink_refuses_a_cleartext_non_loopback_url() {
        // A digest carries database names and backup state, so the flag must
        // not be able to put it on the wire in the clear.
        for url in [
            "http://example.com/hook",
            "ftp://127.0.0.1/hook",
            "not a url",
        ] {
            assert!(
                WebhookSink::new(url).is_err(),
                "{url} must not be accepted as a notification sink"
            );
        }
        assert!(WebhookSink::new("http://127.0.0.1:9/hook").is_ok());
        assert!(WebhookSink::new("https://hooks.example.com/dg").is_ok());
    }
}
