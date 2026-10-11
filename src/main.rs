use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::OpenOptions,
    io::{BufReader, IsTerminal, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

/// Stage an export download next to its destination so publication below is
/// a same-directory atomic no-clobber link. Chunks stream in one at a time;
/// only a single chunk is ever buffered in memory.
fn export_stage_file(out: &Path) -> dg::error::Result<tempfile::NamedTempFile> {
    let parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    tempfile::NamedTempFile::new_in(parent).map_err(|_| dg::error::Error::internal())
}

fn publish_export_file(temp: tempfile::NamedTempFile, path: &Path) -> dg::error::Result<()> {
    temp.as_file()
        .sync_all()
        .map_err(|_| dg::error::Error::internal())?;
    // A same-directory hard link is an atomic no-clobber publication on the
    // supported Unix deployment. The temporary link is cleaned on return.
    // The parent directory fsync makes the new name itself durable: without
    // it a crash can lose the published file even though its bytes are safe.
    std::fs::hard_link(temp.path(), path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            dg::error::Error::new(409, "file_exists", "destination file already exists")
        } else {
            dg::error::Error::internal()
        }
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .is_err()
    {
        // The artifact is already visible at `path`. Reporting failure while
        // leaving it there tells the operator the export did not happen when
        // it did, and the stale file then blocks the next export. Undo the
        // link so the reported state and the on-disk state agree.
        let _ = std::fs::remove_file(path);
        return Err(dg::error::Error::internal());
    }
    Ok(())
}

fn api_failed(response: &Value) -> bool {
    response.get("error").is_some()
}

fn api_error() -> dg::error::Error {
    dg::error::Error::new(400, "api_error", "API request failed; see JSON response")
}

/// Terminal-table fetch shared by the JSON-listing commands. Returns
/// `Ok(None)` when output stays machine JSON (piped stdout or --json);
/// otherwise fetches, renders any error envelope, and hands the JSON
/// back for table shaping (Err on API failure).
async fn table_json(
    cli: &Cli,
    key_path: &Option<PathBuf>,
    method: reqwest::Method,
    path: &str,
) -> Result<Option<Value>, dg::ServerError> {
    use dg::ServerError::Application;
    if cli.json || !std::io::stdout().is_terminal() {
        return Ok(None);
    }
    let key = dg::client::read_key(key_path.as_deref().ok_or_else(|| {
        Application(dg::error::Error::new(
            400,
            "missing_credential",
            "--key-file required",
        ))
    })?)
    .map_err(Application)?;
    let result = dg::client::request(&cli.server, &key, method, path, None, None)
        .await
        .map_err(Application)?;
    if api_failed(&result) {
        dg::shell::render(&result, false, &mut std::io::stdout()).map_err(Application)?;
        return Err(Application(api_error()));
    }
    Ok(Some(result))
}

#[derive(Parser)]
#[command(name = "dg", version, about = "DB Gateway — personal SQLite service")]
struct Cli {
    /// Precedence: --server flag, then DG_SERVER, then the loopback default.
    #[arg(
        long,
        global = true,
        env = "DG_SERVER",
        default_value = "http://127.0.0.1:7432"
    )]
    server: String,
    /// File containing a raw credential (0600 on Unix). Never pass secrets as flags.
    /// Precedence: --key-file flag, then DG_KEY_FILE, then ~/.config/dg/key.
    #[arg(long, global = true, env = "DG_KEY_FILE")]
    key_file: Option<PathBuf>,
    /// Emit JSON (default for all commands except the interactive shell).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Generate the owner locally; emits its credential once. Redirect securely.
    /// The key is also announced on stderr, since stdout alone is easy to lose
    /// to terminal scrollback.
    Init {
        /// Data directory to generate the owner credential for.
        #[arg(long)]
        data_dir: PathBuf,
    },
    /// Run the authenticated server (loopback only; remote access via SSH tunnel).
    Serve {
        /// Loopback address to bind, e.g. 127.0.0.1:7432. Remote access
        /// rides an SSH tunnel; public binds are refused.
        #[arg(long, default_value = "127.0.0.1:7432")]
        listen: SocketAddr,
        /// Directory holding the registry and database files.
        #[arg(long, default_value = "state/dg")]
        data_dir: PathBuf,
        /// Versioned backup-set root for `/v1/backups` visibility. Defaults
        /// to `<data-dir>/../dg-backups`; unknown when the directory is
        /// missing.
        #[arg(long)]
        backup_root: Option<PathBuf>,
        /// Deliver the backup-health digest on this cadence, in seconds
        /// (observability contract §3 alert 4). 0 disables the digest and
        /// leaves only immediate failure notices. The log sink is always on;
        /// the default is off, because a notification is an opt-in.
        #[arg(long, default_value_t = 0)]
        backup_digest_secs: u64,
        /// How often to check the producer's attempt log for a new failure,
        /// in seconds. 0 disables the immediate notice. Independent of the
        /// digest cadence: a failure is news, freshness is a daily question.
        #[arg(long, default_value_t = 0)]
        backup_failure_poll_secs: u64,
        /// Also POST each notification as JSON to this URL, in addition to
        /// the log sink. HTTPS, or plain HTTP on a literal loopback address.
        /// Never read from the environment: a notification that starts
        /// leaving the host because a unit file changed is not a change
        /// anyone reviewed.
        #[arg(long)]
        backup_notify_webhook: Option<String>,
    },
    /// Owner-only service status: version, uptime, admission config, and
    /// request-pressure counters.
    Status,
    /// List database names visible to the credential (owner sees all).
    List,
    /// Create an empty database.
    Create {
        /// Name for the new database: lowercase letters, digits, and `_`,
        /// at most 64 chars, never starting with `_`.
        name: String,
    },
    /// Show size, tables, creation time, and tombstone state of a database.
    Info {
        /// Database to describe.
        name: String,
    },
    /// Clone a database into a new one via the SQLite backup API.
    Clone {
        /// Existing database to copy from.
        source: String,
        /// Name for the new copy; must not exist yet.
        destination: String,
    },
    /// Snapshot a database to a local .sqlite file (owner/app key).
    Export {
        /// Database to snapshot.
        name: String,
        /// Local .sqlite path to write. An existing file is refused and the
        /// download is published atomically.
        #[arg(long, required = true)]
        out: PathBuf,
    },
    /// Import a local .sqlite file as a new database (owner key only).
    Import {
        /// Name for the imported database; must not exist yet.
        name: String,
        /// Regular .sqlite file to upload; 128 MiB ceiling.
        #[arg(long, required = true)]
        from: PathBuf,
    },
    /// Tombstone a database; file purged after the retention window.
    Delete {
        name: String,
        /// Type the exact database name again to confirm deletion.
        #[arg(long, value_name = "NAME")]
        confirm: Option<String>,
    },
    /// Interactive SQL session over the HTTP API (piped stdin works too).
    /// An optional database name preselects it, like `turso db shell <name>`;
    /// `use <db>` inside still switches.
    Shell {
        /// Database to preselect, like `turso db shell <name>`. Inside the
        /// session `use <db>` still switches.
        name: Option<String>,
    },
    /// Execute one SQL statement and print the result rows.
    Query {
        /// Database to run the statement against.
        name: String,
        /// One SQL statement to execute.
        sql: String,
        /// JSON array of bind parameters, e.g. '[7,"a"]'. Defaults to [].
        #[arg(long, default_value = "[]")]
        args: String,
        /// Replay key: a retry with the same key and identical body returns
        /// the stored first response instead of re-running the statement.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Execute an atomic JSON batch from a file. The file is read whole and is capped at 64 KiB.
    Batch {
        /// Database to run the batch against.
        name: String,
        /// Batch file: {"statements":[{"sql":"...","args":[]}]}.
        file: PathBuf,
        /// Replay key: a retry with the same key and identical body returns
        /// the stored first response instead of re-running the batch.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Manage scoped API credentials (create, list, revoke, rotate owner).
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Backup freshness and offline backup-set verification.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Start the workstation web console on loopback and print its URL.
    /// Forwards the resolved --server/--key-file values to the companion
    /// and supervises it; the owner credential itself never passes
    /// through this process. When the loopback gateway at --server is
    /// unreachable, a `dg serve` child is started on it automatically
    /// (same data dir default as `serve`) and supervised alongside.
    Web {
        /// Loopback port for the browser console.
        /// Precedence: flag, DG_CONSOLE_PORT, 8787.
        #[arg(long, env = "DG_CONSOLE_PORT", default_value = "8787")]
        port: u16,
        /// Companion entrypoint override. Default: web/dist/server.js
        /// resolved from the dg executable or working directory.
        #[arg(long, env = "DG_CONSOLE_ENTRY")]
        entry: Option<PathBuf>,
        /// Data directory for the auto-started gateway. Used only when
        /// --server is unreachable; an already-running gateway is used
        /// as-is and this is ignored.
        #[arg(long, default_value = "state/dg")]
        data_dir: PathBuf,
    },
}
#[derive(Subcommand)]
enum KeyCommand {
    /// List outstanding credentials with scope and role (hashes never shown).
    List,
    /// Create a scoped credential; prints the raw key once.
    Create {
        /// Name for the new credential.
        name: String,
        /// Access level: database_read_only or database_read_write.
        #[arg(long,value_parser=["database_read_only","database_read_write"])]
        role: String,
        /// Database this key may reach (repeatable; at least one).
        #[arg(long, required = true)]
        database: Vec<String>,
    },
    /// Revoke a credential immediately.
    Revoke { name: String },
    /// Replace the owner credential; prints the new key once (old key dies
    /// immediately). Redirect to a 0600 file and verify before discarding the old.
    RotateOwner,
}
#[derive(Subcommand)]
enum BackupCommand {
    /// Owner-only backup freshness: versioned sets plus per-database
    /// fresh/stale/unknown state.
    Status,
    /// The daily backup-health digest, exactly as the notification sink
    /// renders it: per-database fresh/stale/unknown, newest verified set,
    /// age, and a bounded operator action. Reads the data directory and the
    /// backup root directly, so it works with the gateway stopped -- which
    /// is when an operator most wants it. Exits nonzero when any database
    /// is stale or unknown.
    Digest {
        /// Data directory to read durable write markers from.
        #[arg(long, default_value = "state/dg")]
        data_dir: PathBuf,
        /// Backup root holding versioned sets. Defaults to
        /// `<data-dir>/../dg-backups`, the same convention `serve` uses.
        #[arg(long)]
        backup_root: Option<PathBuf>,
        /// Emit the structured note instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Re-verify a published backup set offline: manifest schema, hash
    /// chain, integrity, foreign keys, and protected tables. Prints a
    /// JSON report; exits nonzero when any check fails.
    Verify {
        /// Backup root holding versioned sets.
        #[arg(long, required = true)]
        backup_root: PathBuf,
        /// Set timestamp, or "latest" (default).
        #[arg(default_value = "latest")]
        set: String,
    },
}
/// Resolve the console companion entrypoint: explicit override first,
/// then web/dist/server.js beside the checkout (from the executable or
/// the working directory).
fn console_entry(explicit: Option<PathBuf>) -> Result<PathBuf, dg::error::Error> {
    use dg::error::Error;
    // TRUST BOUNDARY (owner decision 2026-09-27): the last candidate below is
    // resolved relative to the process working directory and handed straight to
    // `node`, so `dg web` run inside an untrusted checkout executes that
    // directory's JavaScript with the operator's privileges. This is the
    // intended repo-local developer workflow and matches any Node-based dev
    // tool, so the fallback is kept deliberately. The two exe-relative
    // candidates are preferred, so an installed dg uses the companion shipped
    // beside it. An operator who needs the stricter posture should pass
    // --entry explicitly.
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path);
        }
        eprintln!("console companion not found: {}", path.display());
        return Err(Error::new(
            400,
            "invalid_console_entry",
            "console companion entrypoint does not exist",
        ));
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("../../web/dist/server.js"));
        candidates.push(dir.join("../web/dist/server.js"));
    }
    candidates.push(PathBuf::from("web/dist/server.js"));
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    for candidate in &candidates {
        eprintln!("console companion not found: {}", candidate.display());
    }
    Err(Error::new(
        400,
        "invalid_console_entry",
        "console companion not found; run `npm run build --prefix web` first",
    ))
}

/// Stop an auto-started gateway during `dg web` shutdown. No child means
/// the operator's own gateway is in use; it is never touched.
async fn stop_gateway(gateway: &mut Option<tokio::process::Child>) {
    let Some(child) = gateway.as_mut() else {
        return;
    };
    #[cfg(unix)]
    terminate_child(child).await;
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
    *gateway = None;
}

/// Ask the companion to stop gracefully (it closes its listener and any
/// managed SSH tunnel), escalating only if it ignores SIGTERM.
#[cfg(unix)]
async fn terminate_child(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Best effort: the child may already be gone.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// Render a `host:port` authority for a socket address, bracketing an IPv6
/// literal. A bare `::1:8787` does not parse as a `SocketAddr` and is not a
/// valid URL authority either; `[::1]:8787` is both. A host that already
/// carries brackets (as `Url::host_str` returns for IPv6) is left alone.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Loopback-only check mirroring the companion bind gate (web/src/server.ts
/// isLoopback): any 127/8 address, the IPv6 loopback (bracketed or bare),
/// or localhost. Anything else must not become a bind address, probe target,
/// or printed URL.
fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim().to_lowercase();
    if normalized == "localhost" || normalized == "::1" || normalized == "[::1]" {
        return true;
    }
    let parts: Vec<&str> = normalized.split('.').collect();
    parts.len() == 4
        && parts[0] == "127"
        && parts[1..].iter().all(|octet| octet.parse::<u8>().is_ok())
}

/// Loopback-http gateway target (`host:port`) we may auto-start a server
/// on. Anything else (https, non-loopback, unparsable) is left to the
/// companion to report; this process never spawns remote listeners.
fn gateway_target(server: &str) -> Option<String> {
    let url = reqwest::Url::parse(server).ok()?;
    if url.scheme() != "http" || url.path() != "/" {
        return None;
    }
    let host = url.host_str()?;
    let ip: std::net::IpAddr = host.trim_matches(['[', ']']).parse().ok()?;
    if !ip.is_loopback() {
        return None;
    }
    Some(authority(host, url.port().unwrap_or(80)))
}

/// Wait until `target` accepts TCP connections, the child exits, or the
/// deadline passes. An accepted connection means the service bound and is
/// past credential loading; details go to stderr, codes stay static.
async fn wait_for_listener(
    target: &str,
    child: &mut tokio::process::Child,
    timeout: Duration,
    service: &str,
    code: &'static str,
    exit_message: &'static str,
    timeout_message: &'static str,
) -> Result<(), dg::error::Error> {
    use dg::error::Error;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(target).await.is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|_| Error::internal())? {
            eprintln!("{service} exited before becoming ready: {status}");
            return Err(Error::new(400, code, exit_message));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(Error::new(400, code, timeout_message));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Auto-serve leg of `dg web`: when the loopback gateway is unreachable,
/// start `dg serve` on it (this same executable) and wait for readiness.
/// Returns the supervised child, or `None` when a gateway is already up
/// or the target is not auto-startable.
async fn ensure_gateway(
    server: &str,
    data_dir: &Path,
) -> Result<Option<tokio::process::Child>, dg::error::Error> {
    use dg::error::Error;
    let Some(target) = gateway_target(server) else {
        return Ok(None);
    };
    if tokio::net::TcpStream::connect(&target).await.is_ok() {
        return Ok(None);
    }
    let exe = std::env::current_exe().map_err(|_| Error::internal())?;
    let mut child = tokio::process::Command::new(exe)
        .args(["serve", "--listen", &target, "--data-dir"])
        .arg(data_dir)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|_| Error::internal())?;
    wait_for_listener(
        &target,
        &mut child,
        Duration::from_secs(15),
        "auto-started gateway",
        "gateway_startup_failed",
        "local gateway exited before becoming ready; run `dg serve` manually to diagnose",
        "local gateway did not become ready in time",
    )
    .await?;
    eprintln!(
        "dg web: no gateway at {server}; started local gateway on {target} (data dir {})",
        data_dir.display()
    );
    Ok(Some(child))
}

/// Start the workstation console companion, print its loopback URL once
/// it accepts connections, and supervise it until it exits or we are
/// signalled. Only the key *path* is forwarded; key material is never
/// read here. Remaining environment (notably DG_SSH_*) passes through
/// untouched so managed tunnels keep working.
async fn run_console(
    server: &str,
    key_path: Option<PathBuf>,
    port: u16,
    entry: Option<PathBuf>,
    data_dir: PathBuf,
) -> Result<(), dg::error::Error> {
    use dg::error::Error;
    if !(1024..=65535).contains(&port) {
        return Err(Error::new(
            400,
            "invalid_console_port",
            "console port must be 1024-65535",
        ));
    }
    let key_path = key_path.ok_or(Error::new(400, "missing_credential", "--key-file required"))?;
    let console_host = std::env::var("DG_CONSOLE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    // The value becomes the companion bind address, the readiness-probe
    // target, and the printed URL. Refuse anything the companion would not
    // bind (server.ts isLoopback) here, before a hostname can turn the
    // probe into a TCP connection to an arbitrary host.
    if !is_loopback_host(&console_host) {
        return Err(Error::new(
            400,
            "invalid_console_host",
            "DG_CONSOLE_HOST must be a loopback address",
        ));
    }
    let entry = console_entry(entry)?;
    let mut gateway = ensure_gateway(server, &data_dir).await?;
    // Past this point every exit path must stop the auto-started gateway:
    // tokio's Child has no kill_on_drop, so an early `?` orphans a `dg serve`
    // that keeps holding the data dir and the port. This is most reachable
    // when node is missing, which fails the spawn below.
    let mut command = tokio::process::Command::new("node");
    command
        .arg(&entry)
        .env("DG_SERVER", server)
        .env("DG_KEY_FILE", &key_path)
        .env("DG_CONSOLE_PORT", port.to_string())
        .stdin(std::process::Stdio::null());
    if std::env::var_os("DG_CONSOLE_HOST").is_none() {
        command.env("DG_CONSOLE_HOST", &console_host);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            stop_gateway(&mut gateway).await;
            return Err(if error.kind() == std::io::ErrorKind::NotFound {
                Error::new(
                    400,
                    "console_runtime_unavailable",
                    "node executable not found on PATH; the console companion requires node",
                )
            } else {
                Error::internal()
            });
        }
    };
    // The companion only binds after loading the credential and starting
    // any managed tunnel, so an accepted connection means ready. Probe the
    // configured host: a fixed 127.0.0.1 never reaches a ::1 listener.
    let address = authority(&console_host, port);
    if let Err(error) = wait_for_listener(
        &address,
        &mut child,
        Duration::from_secs(30),
        "console companion",
        "console_startup_failed",
        "console companion exited before becoming ready; see output above",
        "console companion did not become ready in time",
    )
    .await
    {
        stop_gateway(&mut gateway).await;
        terminate_child(&mut child).await;
        let _ = child.wait().await;
        return Err(error);
    }
    println!("dg web ready: open http://{address} in a browser");
    if std::io::stdout().flush().is_err() {
        stop_gateway(&mut gateway).await;
        terminate_child(&mut child).await;
        let _ = child.wait().await;
        return Err(Error::internal());
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                stop_gateway(&mut gateway).await;
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                return Err(Error::internal());
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(_) => {
                stop_gateway(&mut gateway).await;
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                return Err(Error::internal());
            }
        };
        let gateway_exited = async {
            match gateway.as_mut() {
                Some(child) => {
                    let _ = child.wait().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            status = child.wait() => {
                stop_gateway(&mut gateway).await;
                let status = status.map_err(|_| Error::internal())?;
                if status.success() {
                    Ok(())
                } else {
                    eprintln!("console companion exited: {status}");
                    Err(Error::new(
                        500,
                        "console_companion_failed",
                        "console companion exited with an error",
                    ))
                }
            }
            // Shutdown was operator-initiated; the companion's
            // signal-induced exit status is still a clean stop.
            _ = sigterm.recv() => {
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                stop_gateway(&mut gateway).await;
                Ok(())
            }
            _ = sigint.recv() => {
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                stop_gateway(&mut gateway).await;
                Ok(())
            }
            // Our gateway died under us; the console is useless without
            // it, so stop the companion instead of serving errors.
            _ = gateway_exited => {
                eprintln!("auto-started gateway exited; stopping console");
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                Err(Error::new(
                    500,
                    "gateway_exited",
                    "auto-started gateway exited; console stopped",
                ))
            }
        }
    }
    #[cfg(not(unix))]
    {
        let status = child.wait().await.map_err(|_| Error::internal())?;
        stop_gateway(&mut gateway).await;
        if status.success() {
            Ok(())
        } else {
            eprintln!("console companion exited: {status}");
            Err(Error::new(
                500,
                "console_companion_failed",
                "console companion exited with an error",
            ))
        }
    }
}

async fn run(cli: Cli) -> Result<(), dg::ServerError> {
    use dg::{ServerError::Application, error::Error};
    use reqwest::Method;
    // Credential resolution: explicit flag, env, then the default key file.
    // Missing everywhere is the same local error as before.
    let key_path = cli.key_file.clone().or_else(dg::client::default_key_file);
    let (method, path, body, idempotency) = match cli.command {
        Command::Serve {
            listen,
            data_dir,
            backup_root,
            backup_digest_secs,
            backup_failure_poll_secs,
            backup_notify_webhook,
        } => {
            // A flag that cannot be honoured is refused at startup rather
            // than half-applied: a sink silently dropped means an operator
            // believes they have webhook delivery and do not.
            // A webhook alone schedules nothing: delivery only runs on the
            // digest interval or the failure poll, so a bare webhook would
            // log "sink added" and then never fire. Refuse it with the
            // remedy instead of announcing a sink that cannot deliver.
            if backup_notify_webhook.is_some()
                && backup_digest_secs == 0
                && backup_failure_poll_secs == 0
            {
                return Err(Application(dg::error::Error::new(
                    400,
                    "invalid_backup_webhook",
                    "--backup-notify-webhook requires --backup-digest-secs or --backup-failure-poll-secs; a webhook alone never fires",
                )));
            }
            let wants_delivery = backup_digest_secs > 0
                || backup_failure_poll_secs > 0
                || backup_notify_webhook.is_some();
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
            return dg::serve(listen, data_dir, backup_root, notify).await;
        }
        Command::Status => {
            if let Some(status) = table_json(&cli, &key_path, Method::GET, "/v1/status").await? {
                let mut rows = vec![
                    json!(["version", status["version"]]),
                    json!(["uptime_secs", status["uptime_secs"]]),
                    json!([
                        "acquire_timeout_ms",
                        status["admission"]["acquire_timeout_ms"]
                    ]),
                    json!(["read_permits", status["admission"]["read_permits"]]),
                    json!(["write_permits", status["admission"]["write_permits"]]),
                    json!(["requests_total", status["requests"]["total"]]),
                    json!(["capacity_exceeded", status["requests"]["capacity_exceeded"]]),
                    json!([
                        "deadline_exceeded",
                        status["requests"]["query_deadline_exceeded"]
                    ]),
                    json!(["internal_error", status["requests"]["internal_error"]]),
                ];
                if let Some(usage) = status["databases"].as_object() {
                    let mut names: Vec<&String> = usage.keys().collect();
                    names.sort();
                    for name in names {
                        let entry = &usage[name.as_str()];
                        rows.push(json!([
                            format!("usage {name}"),
                            format!(
                                "read {}, written {}, queue {} waits/{}ms/{} timeouts",
                                entry["rows_read"].as_u64().unwrap_or(0),
                                entry["rows_written"].as_u64().unwrap_or(0),
                                entry["queue_waits"].as_u64().unwrap_or(0),
                                entry["queue_wait_ms"].as_u64().unwrap_or(0),
                                entry["queue_timeouts"].as_u64().unwrap_or(0),
                            )
                        ]));
                    }
                }
                dg::shell::render(
                    &json!({"columns":["property","value"],"rows":rows}),
                    false,
                    &mut std::io::stdout(),
                )
                .map_err(Application)?;
                return Ok(());
            }
            (Method::GET, "/v1/status".into(), None, None)
        }
        Command::Backup {
            command: BackupCommand::Status,
        } => {
            if let Some(backups) = table_json(&cli, &key_path, Method::GET, "/v1/backups").await? {
                let mut rows = vec![
                    json!(["backup_root", backups["backup_root"]]),
                    json!(["stale_after_secs", backups["stale_after_secs"]]),
                ];
                if let Some(sets) = backups["sets"].as_array() {
                    rows.push(json!([
                        "sets",
                        if sets.is_empty() {
                            "none".to_string()
                        } else {
                            sets.iter()
                                .filter_map(|set| set["timestamp"].as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ]));
                }
                if let Some(per_database) = backups["per_database"].as_object() {
                    let mut names: Vec<&String> = per_database.keys().collect();
                    names.sort();
                    for name in names {
                        let fresh = &per_database[name.as_str()];
                        rows.push(json!([
                            format!("backup {name}"),
                            format!(
                                "{}{}{}",
                                fresh["state"].as_str().unwrap_or("unknown"),
                                fresh["newest_set"]
                                    .as_str()
                                    .map(|set| format!(" @ {set}"))
                                    .unwrap_or_default(),
                                fresh["age_secs"]
                                    .as_u64()
                                    .map(|age| format!(" ({age}s ago)"))
                                    .unwrap_or_default()
                            )
                        ]));
                    }
                }
                dg::shell::render(
                    &json!({"columns":["property","value"],"rows":rows}),
                    false,
                    &mut std::io::stdout(),
                )
                .map_err(Application)?;
                return Ok(());
            }
            (Method::GET, "/v1/backups".into(), None, None)
        }
        Command::Backup {
            command:
                BackupCommand::Digest {
                    data_dir,
                    backup_root,
                    json: as_json,
                },
        } => {
            // Deliberately local, not an HTTP round trip: the digest an
            // operator reads while the gateway is down or restarting is the
            // one they need most, and asking a wedged process for its own
            // health report is a bad way to deliver it. Same rule, same
            // `snapshot`, so this and the sink cannot disagree.
            let store = dg::store::Store::open(&data_dir).map_err(Application)?;
            let store = match backup_root {
                Some(root) => store.with_backup_root(Some(root)),
                None => store,
            };
            let health = dg::backup_health::snapshot(&store).map_err(Application)?;
            let note = dg::notify::digest_note(&health);
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&note)
                        .map_err(|_| Application(dg::error::Error::internal()))?
                );
            } else {
                let mut rows: Vec<Value> = vec![json!(["state", note.title.clone()])];
                for db in &health.databases {
                    rows.push(json!([
                        format!("backup {}", db.name),
                        format!(
                            "{}{}{}",
                            db.state,
                            db.newest_set
                                .as_deref()
                                .map(|set| format!(" @ {set}"))
                                .unwrap_or_default(),
                            db.age_secs
                                .map(|age| format!(" ({age}s ago)"))
                                .unwrap_or_default()
                        )
                    ]));
                }
                if let Some(failure) = &health.last_failure {
                    rows.push(json!([
                        "last failure",
                        format!(
                            "{} at step {}",
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
                        )
                    ]));
                }
                rows.push(json!(["action", note.action.clone()]));
                dg::shell::render(
                    &json!({"columns":["property","value"],"rows":rows}),
                    false,
                    &mut std::io::stdout(),
                )
                .map_err(Application)?;
            }
            // Alert 4 is a condition, not a report: a digest that exits 0
            // while a database is unrepresented is how a schedule gets
            // mistaken for a backup. `unknown` fails too, because §2 says
            // unknown is the state that keeps a scheduled job honest.
            if health.attention().is_empty() {
                return Ok(());
            }
            return Err(Application(dg::error::Error::new(
                1,
                "backup_unhealthy",
                "at least one database is stale or has no verified copy",
            )));
        }
        Command::Backup {
            command: BackupCommand::Verify { backup_root, set },
        } => {
            let report = dg::backup::verify_set(&backup_root, &set).map_err(Application)?;
            println!(
                "{}",
                serde_json::to_value(&report).map_err(|_| Application(Error::internal()))?
            );
            if report.ok {
                return Ok(());
            }
            return Err(Application(Error::new(
                400,
                "backup_verification_failed",
                "backup set failed verification; see JSON report",
            )));
        }
        Command::Web {
            port,
            entry,
            data_dir,
        } => {
            return run_console(&cli.server, key_path, port, entry, data_dir)
                .await
                .map_err(Application);
        }
        Command::Init { data_dir } => {
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
            println!("{}", json!({"owner_key":key}));
            return Ok(());
        }
        Command::List => {
            // Interactive terminals get an aligned table; piped output
            // and --json stay machine-readable JSON.
            if !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
                    Error::new(400, "missing_credential", "--key-file required"),
                ))?)
                .map_err(Application)?;
                let result = dg::client::request(
                    &cli.server,
                    &key,
                    Method::GET,
                    "/v1/databases",
                    None,
                    None,
                )
                .await
                .map_err(Application)?;
                if api_failed(&result) {
                    dg::shell::render(&result, false, &mut std::io::stdout())
                        .map_err(Application)?;
                    return Err(Application(api_error()));
                }
                let rows: Vec<Value> = result["databases"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|name| json!([name]))
                    .collect();
                dg::shell::render(
                    &json!({"columns":["name"],"rows":rows}),
                    false,
                    &mut std::io::stdout(),
                )
                .map_err(Application)?;
                return Ok(());
            }
            (Method::GET, "/v1/databases".into(), None, None)
        }
        Command::Info { name } => {
            dg::store::validate_name(&name).map_err(Application)?;
            if !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
                    Error::new(400, "missing_credential", "--key-file required"),
                ))?)
                .map_err(Application)?;
                let info = dg::client::request(
                    &cli.server,
                    &key,
                    Method::GET,
                    &format!("/v1/databases/{name}"),
                    None,
                    None,
                )
                .await
                .map_err(Application)?;
                if api_failed(&info) {
                    dg::shell::render(&info, false, &mut std::io::stdout()).map_err(Application)?;
                    return Err(Application(api_error()));
                }
                // Usage is auxiliary: a failed status fetch degrades the
                // two usage cells instead of failing the command. Absent
                // from a good status means true zero activity.
                let (rows_read, rows_written) =
                    dg::client::request(&cli.server, &key, Method::GET, "/v1/status", None, None)
                        .await
                        .ok()
                        .filter(|status| !api_failed(status))
                        .map(|status| {
                            let use_ = &status["databases"][&name];
                            (
                                use_["rows_read"].as_u64().unwrap_or(0).to_string(),
                                use_["rows_written"].as_u64().unwrap_or(0).to_string(),
                            )
                        })
                        .unwrap_or_else(|| ("unavailable".to_string(), "unavailable".to_string()));
                let mut rows = vec![
                    json!(["name", info["name"]]),
                    json!(["tombstoned", info["tombstoned"]]),
                    json!(["size_bytes", info["size_bytes"]]),
                    json!(["rows_read", rows_read]),
                    json!(["rows_written", rows_written]),
                ];
                if info["tombstoned"].as_bool() == Some(true) {
                    rows.push(json!(["purge_after", info["purge_after"]]));
                } else {
                    rows.push(json!(["created_at", info["created_at"]]));
                    let tables = info["tables"]
                        .as_array()
                        .map(|tables| {
                            tables
                                .iter()
                                .map(|t| {
                                    format!(
                                        "{} ({} rows)",
                                        t["name"].as_str().unwrap_or("?"),
                                        t["rows"].as_u64().unwrap_or(0)
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    rows.push(json!(["tables", tables]));
                }
                dg::shell::render(
                    &json!({"columns":["property","value"],"rows":rows}),
                    false,
                    &mut std::io::stdout(),
                )
                .map_err(Application)?;
                return Ok(());
            }
            (Method::GET, format!("/v1/databases/{name}"), None, None)
        }
        Command::Create { name } => {
            dg::store::validate_name(&name).map_err(Application)?;
            (
                Method::POST,
                "/v1/databases".into(),
                Some(json!({"name":name})),
                None,
            )
        }
        Command::Clone {
            source,
            destination,
        } => {
            dg::store::validate_name(&source).map_err(Application)?;
            dg::store::validate_name(&destination).map_err(Application)?;
            (
                Method::POST,
                format!("/v1/databases/{source}/clone"),
                Some(json!({"destination":destination})),
                None,
            )
        }
        Command::Export { name, out } => {
            dg::store::validate_name(&name).map_err(Application)?;
            let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
                400,
                "missing_credential",
                "--key-file required",
            )))?)
            .map_err(Application)?;
            // Chunked download: freeze a snapshot, stream it piece by piece,
            // verify the whole-file hash, then publish atomically.
            let begin = dg::client::request(
                &cli.server,
                &key,
                Method::POST,
                &format!("/v1/databases/{name}/export/begin"),
                Some(json!({})),
                None,
            )
            .await
            .map_err(Application)?;
            if api_failed(&begin) {
                println!("{begin}");
                return Err(Application(api_error()));
            }
            let snapshot = begin["snapshot"]
                .as_str()
                .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .ok_or(Application(Error::internal()))?
                .to_string();
            let release_snapshot = || async {
                let _ = dg::client::request(
                    &cli.server,
                    &key,
                    Method::DELETE,
                    &format!("/v1/snapshots/{snapshot}"),
                    None,
                    None,
                )
                .await;
            };
            let result = async {
                // The server has already retained the snapshot named by
                // begin. Keep every subsequent validation and transfer
                // operation inside the cleanup guard so malformed metadata
                // cannot consume a lifecycle slot until the TTL.
                let size = begin["size"]
                    .as_u64()
                    .filter(|size| *size <= dg::store::MAX_TRANSFER_SIZE)
                    .ok_or_else(|| {
                        Application(Error::new(
                            413,
                            "result_limit_exceeded",
                            "export exceeds bounded response size",
                        ))
                    })?;
                let sha256 = begin["sha256"]
                    .as_str()
                    .filter(|hash| {
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .ok_or(Application(Error::internal()))?
                    .to_string();
                let mut staged = export_stage_file(&out).map_err(Application)?;
                let mut hasher = Sha256::new();
                let mut offset = 0u64;
                while offset < size {
                    let chunk = dg::client::request(
                        &cli.server,
                        &key,
                        Method::GET,
                        &format!(
                            "/v1/snapshots/{snapshot}?offset={offset}&length={}",
                            dg::store::MAX_CHUNK
                        ),
                        None,
                        None,
                    )
                    .await
                    .map_err(Application)?;
                    if api_failed(&chunk) {
                        println!("{chunk}");
                        return Err(Application(api_error()));
                    }
                    if chunk["snapshot"].as_str() != Some(snapshot.as_str())
                        || chunk["size"].as_u64() != Some(size)
                        || chunk["sha256"].as_str() != Some(sha256.as_str())
                        || chunk["offset"].as_u64() != Some(offset)
                    {
                        return Err(Application(Error::internal()));
                    }
                    let encoded = chunk["data_base64"]
                        .as_str()
                        .ok_or(Application(Error::internal()))?;
                    let bytes =
                        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                            .map_err(|_| Application(Error::internal()))?;
                    if bytes.is_empty()
                        || bytes.len() > dg::store::MAX_CHUNK
                        || bytes.len() as u64 > size.saturating_sub(offset)
                    {
                        return Err(Application(Error::internal()));
                    }
                    staged
                        .write_all(&bytes)
                        .map_err(|_| Application(Error::internal()))?;
                    hasher.update(&bytes);
                    offset += bytes.len() as u64;
                }
                if offset != size || hex::encode(hasher.finalize()) != sha256 {
                    return Err(Application(Error::internal()));
                }
                publish_export_file(staged, &out).map_err(Application)?;
                Ok((size, sha256))
            }
            .await;
            release_snapshot().await;
            let (size, sha256) = result?;
            println!(
                "{}",
                json!({"name":name,"out":out.display().to_string(),"size":size,"sha256":sha256})
            );
            return Ok(());
        }
        Command::Import { name, from } => {
            dg::store::validate_name(&name).map_err(Application)?;
            let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
                400,
                "missing_credential",
                "--key-file required",
            )))?)
            .map_err(Application)?;
            // Chunked upload: stream the file piece by piece, then commit.
            // Only a single chunk is ever buffered in memory.
            // Open with O_NONBLOCK (like the batch path) so a writer-less
            // FIFO cannot hang the CLI before the regular-file check below.
            #[cfg(unix)]
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&from)
                .map_err(|_| Application(Error::invalid()))?;
            #[cfg(not(unix))]
            let file = std::fs::File::open(&from).map_err(|_| Application(Error::invalid()))?;
            let metadata = file.metadata().map_err(|_| Application(Error::invalid()))?;
            // A directory, FIFO, or device is a bad path, not a size problem.
            // Folding the type check into the 413 below answered "too large"
            // for input the sibling `dg batch` path calls invalid.
            if !metadata.is_file() {
                return Err(Application(Error::invalid()));
            }
            if metadata.len() > dg::store::MAX_TRANSFER_SIZE {
                return Err(Application(Error::new(
                    413,
                    "result_limit_exceeded",
                    "import exceeds bounded local file size",
                )));
            }
            let begin = dg::client::request(
                &cli.server,
                &key,
                Method::POST,
                &format!("/v1/databases/{name}/import/begin"),
                Some(json!({})),
                None,
            )
            .await
            .map_err(Application)?;
            if api_failed(&begin) {
                println!("{begin}");
                return Err(Application(api_error()));
            }
            let upload = begin["upload"]
                .as_str()
                .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .ok_or(Application(Error::internal()))?
                .to_string();
            let mut reader = BufReader::new(file);
            let mut buf = vec![0u8; dg::store::MAX_CHUNK];
            let mut offset = 0u64;
            let upload_result = async {
                loop {
                    let n = reader
                        .read(&mut buf)
                        .map_err(|_| Application(Error::invalid()))?;
                    if n == 0 {
                        break;
                    }
                    // The size check at open time is a one-shot sample of a
                    // path the caller controls: the file can grow while we
                    // read it, and a pseudo-file can report a small length and
                    // then stream indefinitely. Enforce the ceiling on the
                    // running total so the bound the error message promises
                    // is a real bound, not just the server's per-append check.
                    if offset + n as u64 > dg::store::MAX_TRANSFER_SIZE {
                        return Err(Application(Error::new(
                            413,
                            "result_limit_exceeded",
                            "import exceeds bounded local file size",
                        )));
                    }
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &buf[..n],
                    );
                    let ack = dg::client::request(
                        &cli.server,
                        &key,
                        Method::POST,
                        &format!("/v1/uploads/{upload}/chunk"),
                        Some(json!({"offset":offset,"data_base64":encoded})),
                        None,
                    )
                    .await
                    .map_err(Application)?;
                    if api_failed(&ack) {
                        println!("{ack}");
                        return Err(Application(api_error()));
                    }
                    offset += n as u64;
                    if ack["received"].as_u64() != Some(offset) {
                        return Err(Application(Error::internal()));
                    }
                }
                let done = dg::client::request(
                    &cli.server,
                    &key,
                    Method::POST,
                    &format!("/v1/uploads/{upload}/commit"),
                    Some(json!({"name":name})),
                    None,
                )
                .await
                .map_err(Application)?;
                println!("{done}");
                if api_failed(&done) {
                    return Err(Application(api_error()));
                }
                Ok::<(), dg::ServerError>(())
            }
            .await;
            if upload_result.is_err() {
                // Abort is best effort: preserve the original import error and
                // never make a cleanup transport failure hide its outcome.
                let _ = dg::client::request(
                    &cli.server,
                    &key,
                    Method::DELETE,
                    &format!("/v1/uploads/{upload}"),
                    None,
                    None,
                )
                .await;
            }
            upload_result?;
            return Ok(());
        }
        Command::Delete { name, confirm } => {
            dg::store::validate_name(&name).map_err(Application)?;
            if confirm.as_deref() != Some(name.as_str()) {
                return Err(Application(Error::new(
                    400,
                    "confirmation_required",
                    "destructive operation: repeat the exact database name with --confirm NAME",
                )));
            }
            (Method::DELETE, format!("/v1/databases/{name}"), None, None)
        }
        Command::Shell { name } => {
            let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
                400,
                "missing_credential",
                "--key-file required",
            )))?)
            .map_err(Application)?;
            if let Some(name) = &name {
                dg::store::validate_name(name).map_err(Application)?;
            }
            return dg::shell::run(
                &cli.server,
                &key,
                cli.json,
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                name.as_deref(),
                std::io::stdin(),
                std::io::stdout(),
            )
            .await
            .map_err(Application);
        }
        Command::Query {
            name,
            sql,
            args,
            idempotency_key,
        } => {
            dg::store::validate_name(&name).map_err(Application)?;
            let args: Vec<Value> =
                serde_json::from_str(&args).map_err(|_| Application(Error::invalid()))?;
            // Interactive terminals get the shell's aligned table; piped
            // output and --json stay machine-readable JSON.
            if !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
                    Error::new(400, "missing_credential", "--key-file required"),
                ))?)
                .map_err(Application)?;
                let result = dg::client::request(
                    &cli.server,
                    &key,
                    Method::POST,
                    &format!("/v1/databases/{name}/query"),
                    Some(json!({"sql":sql,"args":args})),
                    idempotency_key.as_deref(),
                )
                .await
                .map_err(Application)?;
                dg::shell::render(&result, false, &mut std::io::stdout()).map_err(Application)?;
                if result.get("error").is_some() {
                    return Err(Application(Error::new(
                        400,
                        "api_error",
                        "API request failed; see response above",
                    )));
                }
                return Ok(());
            }
            (
                Method::POST,
                format!("/v1/databases/{name}/query"),
                Some(json!({"sql":sql,"args":args})),
                idempotency_key,
            )
        }
        Command::Batch {
            name,
            file,
            idempotency_key,
        } => {
            dg::store::validate_name(&name).map_err(Application)?;
            // Open once with O_NONBLOCK so a writer-less FIFO cannot make
            // the CLI hang before the descriptor is checked. The metadata
            // check is on that same descriptor, so a path replacement
            // cannot turn this into a read of a different file.
            #[cfg(unix)]
            let batch_file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&file);
            #[cfg(not(unix))]
            let batch_file = OpenOptions::new().read(true).open(&file);
            let mut batch_file = batch_file.map_err(|_| Application(Error::invalid()))?;
            if !batch_file
                .metadata()
                .map_err(|_| Application(Error::invalid()))?
                .is_file()
            {
                return Err(Application(Error::invalid()));
            }
            let mut batch_bytes = Vec::new();
            Read::by_ref(&mut batch_file)
                .take(65537)
                .read_to_end(&mut batch_bytes)
                .map_err(|_| Application(Error::invalid()))?;
            if batch_bytes.len() > 65536 {
                return Err(Application(Error::new(
                    413,
                    "input_limit_exceeded",
                    "batch input exceeds 64 KiB",
                )));
            }
            let body: dg::execution::Batch =
                serde_json::from_slice(&batch_bytes).map_err(|_| Application(Error::invalid()))?;
            (
                Method::POST,
                format!("/v1/databases/{name}/batch"),
                Some(serde_json::to_value(body).map_err(|_| Application(Error::invalid()))?),
                idempotency_key,
            )
        }
        Command::Key {
            command:
                KeyCommand::Create {
                    name,
                    role,
                    database,
                },
        } => {
            dg::store::validate_name(&name).map_err(Application)?;
            for db in &database {
                dg::store::validate_name(db).map_err(Application)?;
            }
            (
                Method::POST,
                "/v1/keys".into(),
                Some(json!({"name":name,"role":role,"databases":database})),
                None,
            )
        }
        Command::Key {
            command: KeyCommand::List,
        } => {
            if let Some(keys) = table_json(&cli, &key_path, Method::GET, "/v1/keys").await? {
                let rows: Vec<Value> = keys["keys"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|key| {
                        let databases = key["databases"]
                            .as_array()
                            .map(|dbs| {
                                dbs.iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        json!([
                            key["name"],
                            key["role"],
                            if databases.is_empty() && key["role"].as_str() == Some("owner") {
                                "all".to_string()
                            } else {
                                databases
                            }
                        ])
                    })
                    .collect();
                dg::shell::render(
                    &json!({"columns":["name","role","databases"],"rows":rows}),
                    false,
                    &mut std::io::stdout(),
                )
                .map_err(Application)?;
                return Ok(());
            }
            (Method::GET, "/v1/keys".into(), None, None)
        }
        Command::Key {
            command: KeyCommand::Revoke { name },
        } => {
            dg::store::validate_name(&name).map_err(Application)?;
            (Method::DELETE, format!("/v1/keys/{name}"), None, None)
        }
        Command::Key {
            command: KeyCommand::RotateOwner,
        } => (Method::POST, "/v1/keys/owner/rotate".into(), None, None),
    };
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    let result = dg::client::request(
        &cli.server,
        &key,
        method,
        &path,
        body,
        idempotency.as_deref(),
    )
    .await
    .map_err(Application)?;
    println!("{result}");
    if result.get("error").is_some() {
        return Err(Application(Error::new(
            400,
            "api_error",
            "API request failed; see JSON response",
        )));
    }
    Ok(())
}
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // One `message` key, always: the event message keeps the name and
            // the error's own Display rides a `detail` field. The previous
            // `message=%error` field collided with the event's message key,
            // so a JSON log consumer kept whichever came last and silently
            // lost the other half.
            tracing::error!(code=error.code(), detail=%error, "command failed");
            ExitCode::FAILURE
        }
    }
}
