//! Local-process commands: run the server, or bootstrap a data
//! directory. Neither goes through the HTTP request tail in `run`.

use dg::ServerError::Application;
use serde_json::json;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;

/// How the gateway is to be served.
///
/// The flags that configure one server are a group, not eight loose
/// arguments: a group is what a caller reads, and a call that passes
/// eight of them cannot be checked by eye -- the order of two `Option`
/// arguments and two `u64` arguments is not visible in the call.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The address to bind.
    pub listen: SocketAddr,
    /// Where the data directory is.
    pub data_dir: PathBuf,
    /// The backup root to watch, or the convention default.
    pub backup_root: Option<PathBuf>,
    /// The owner-approved snapshot cadence, in seconds.
    pub backup_cadence_secs: Option<u64>,
    /// How often the backup-health digest is delivered, in seconds.
    pub backup_digest_secs: u64,
    /// How often the producer's attempt log is polled for a new failure.
    pub backup_failure_poll_secs: u64,
    /// The webhook to deliver backup notifications to.
    pub backup_notify_webhook: Option<String>,
    /// Whether the server may outlive the process that launched it.
    pub detach_safe: bool,
}

pub async fn serve(options: ServeOptions) -> Result<(), dg::ServerError> {
    let ServeOptions {
        listen,
        data_dir,
        backup_root,
        backup_cadence_secs,
        backup_digest_secs,
        backup_failure_poll_secs,
        backup_notify_webhook,
        detach_safe,
    } = options;
    // A flag that cannot be honoured is refused at startup rather
    // than half-applied: a sink silently dropped means an operator
    // believes they have webhook delivery and do not.
    // A webhook alone schedules nothing: delivery only runs on the
    // digest interval or the failure poll, so a bare webhook would
    // log "sink added" and then never fire. Refuse it with the
    // remedy instead of announcing a sink that cannot deliver.
    if backup_notify_webhook.is_some() && backup_digest_secs == 0 && backup_failure_poll_secs == 0 {
        return Err(Application(dg::error::Error::new(
            400,
            "invalid_backup_webhook",
            "--backup-notify-webhook requires --backup-digest-secs or --backup-failure-poll-secs; a webhook alone never fires",
        )));
    }
    let wants_delivery =
        backup_digest_secs > 0 || backup_failure_poll_secs > 0 || backup_notify_webhook.is_some();
    let mut sinks: Vec<std::sync::Arc<dyn dg::notify::Sink>> = if wants_delivery {
        // The log sink is the floor: structured logging is already
        // the service's operator channel, so it is the one sink that
        // works with nothing configured anywhere.
        vec![std::sync::Arc::new(dg::notify::LogSink)]
    } else {
        Vec::new()
    };
    if let Some(url) = &backup_notify_webhook {
        let sink = dg::notify::WebhookSink::new(url.clone()).map_err(|error| {
            // A flag the operator wrote is worth a sentence back: a
            // bare `invalid_request` would leave them guessing
            // whether the URL, the scheme or the flag name was wrong.
            // `Error` carries a `&'static str`, so the reason is
            // logged in full here rather than truncated into the
            // response; the response names the flag that is wrong.
            tracing::error!(
                code = "invalid_backup_webhook",
                reason = %error.reason,
                "--backup-notify-webhook was refused at startup"
            );
            Application(dg::error::Error::new(
                400,
                "invalid_backup_webhook",
                "--backup-notify-webhook must be https, or http on a literal loopback address",
            ))
        })?;
        tracing::info!(url = sink.url(), "backup notifications: webhook sink added");
        sinks.push(std::sync::Arc::new(sink));
    }
    let sink_count = sinks.len();
    let notify = dg::BackupNotify {
        sinks,
        digest_interval: std::time::Duration::from_secs(backup_digest_secs),
        failure_poll: std::time::Duration::from_secs(backup_failure_poll_secs),
    };
    if notify.enabled() {
        tracing::info!(
            digest_secs = backup_digest_secs,
            failure_poll_secs = backup_failure_poll_secs,
            sinks = sink_count,
            "backup-health notification delivery enabled"
        );
    }
    dg::serve(
        listen,
        data_dir,
        backup_root,
        backup_cadence_secs,
        notify,
        !detach_safe,
    )
    .await
}

pub async fn init(data_dir: PathBuf, json: bool) -> Result<(), dg::ServerError> {
    let key = dg::store::Store::open(&data_dir)
        .and_then(|store| store.bootstrap())
        .map_err(Application)?;
    // The owner key grants full control of every database, so it must
    // not end up in terminal scrollback or a CI log. stdout still
    // carries it -- an interactive operator needs to read it, and
    // `dg init | tee` must keep working -- but the reminder goes to
    // stderr, where it is visible in a terminal and absent from a
    // redirect.
    eprintln!(
        "dg init: the owner key is on stdout and is shown only once -- redirect it to a mode-0600 file (e.g. `dg init --data-dir DIR > ~/.config/dg/key`) and clear your scrollback."
    );
    if json || !std::io::stdout().is_terminal() {
        // A pipe gets the JSON, so the documented extraction keeps
        // working.
        println!("{}", json!({"owner_key":key}));
        return Ok(());
    }
    // A terminal gets the same key, in a form a person can read and
    // copy: the headline says what happened, the key is on its own
    // line, and the key is on stdout either way.
    let mut out = std::io::stdout();
    dg::render::Outcome::with_facts(
        format!("initialized {}", data_dir.display()),
        vec![("owner key".to_string(), key)],
    )
    .write(&mut out, &dg::render::Theme::for_stream(true))
    .map_err(Application)?;
    Ok(())
}
