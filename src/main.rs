use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
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
    std::fs::hard_link(temp.path(), path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            dg::error::Error::new(409, "file_exists", "destination file already exists")
        } else {
            dg::error::Error::internal()
        }
    })
}

fn api_failed(response: &Value) -> bool {
    response.get("error").is_some()
}

fn api_error() -> dg::error::Error {
    dg::error::Error::new(400, "api_error", "API request failed; see JSON response")
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
    Init {
        #[arg(long)]
        data_dir: PathBuf,
    },
    /// Run the authenticated server (loopback only; remote access via SSH tunnel).
    Serve {
        #[arg(long, default_value = "127.0.0.1:7432")]
        listen: SocketAddr,
        #[arg(long, default_value = "state/dg")]
        data_dir: PathBuf,
        /// Versioned backup-set root for `/v1/backups` visibility. Defaults
        /// to `<data-dir>/../dg-backups`; unknown when the directory is
        /// missing.
        #[arg(long)]
        backup_root: Option<PathBuf>,
    },
    /// Owner-only service status: version, uptime, admission config, and
    /// request-pressure counters.
    Status,
    List,
    Create {
        name: String,
    },
    /// Show size, tables, creation time, and tombstone state of a database.
    Info {
        name: String,
    },
    /// Clone a database into a new one via the SQLite backup API.
    Clone {
        source: String,
        destination: String,
    },
    /// Snapshot a database to a local .sqlite file (owner/app key).
    Export {
        name: String,
        #[arg(long, required = true)]
        out: PathBuf,
    },
    /// Import a local .sqlite file as a new database (owner key only).
    Import {
        name: String,
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
    Shell,
    Query {
        name: String,
        sql: String,
        #[arg(long, default_value = "[]")]
        args: String,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Execute an atomic JSON batch from a file.
    Batch {
        name: String,
        file: PathBuf,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Start the workstation web console on loopback and print its URL.
    /// Forwards the resolved --server/--key-file values to the companion
    /// and supervises it; the owner credential itself never passes
    /// through this process.
    Web {
        /// Loopback port for the browser console.
        /// Precedence: flag, DG_CONSOLE_PORT, 8787.
        #[arg(long, env = "DG_CONSOLE_PORT", default_value = "8787")]
        port: u16,
        /// Companion entrypoint override. Default: web/dist/server.js
        /// resolved from the dg executable or working directory.
        #[arg(long, env = "DG_CONSOLE_ENTRY")]
        entry: Option<PathBuf>,
    },
}
#[derive(Subcommand)]
enum KeyCommand {
    /// List outstanding credentials with scope and role (hashes never shown).
    List,
    Create {
        name: String,
        #[arg(long,value_parser=["database_read_only","database_read_write"])]
        role: String,
        #[arg(long, required = true)]
        database: Vec<String>,
    },
    Revoke {
        name: String,
    },
    /// Replace the owner credential; prints the new key once (old key dies
    /// immediately). Redirect to a 0600 file and verify before discarding the old.
    RotateOwner,
}
#[derive(Subcommand)]
enum BackupCommand {
    /// Owner-only backup freshness: versioned sets plus per-database
    /// fresh/stale/unknown state.
    Status,
}
/// Resolve the console companion entrypoint: explicit override first,
/// then web/dist/server.js beside the checkout (from the executable or
/// the working directory).
fn console_entry(explicit: Option<PathBuf>) -> Result<PathBuf, dg::error::Error> {
    use dg::error::Error;
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
    let entry = console_entry(entry)?;
    let mut command = tokio::process::Command::new("node");
    command
        .arg(&entry)
        .env("DG_SERVER", server)
        .env("DG_KEY_FILE", &key_path)
        .env("DG_CONSOLE_PORT", port.to_string())
        .stdin(std::process::Stdio::null());
    if std::env::var_os("DG_CONSOLE_HOST").is_none() {
        command.env("DG_CONSOLE_HOST", "127.0.0.1");
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::new(
                400,
                "console_runtime_unavailable",
                "node executable not found on PATH; the console companion requires node",
            )
        } else {
            Error::internal()
        }
    })?;
    // The companion only binds after loading the credential and starting
    // any managed tunnel, so an accepted connection means ready.
    let address = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::net::TcpStream::connect(&address).await.is_ok() {
            break;
        }
        if let Some(status) = child.try_wait().map_err(|_| Error::internal())? {
            eprintln!("console companion exited before becoming ready: {status}");
            return Err(Error::new(
                400,
                "console_startup_failed",
                "console companion exited before becoming ready; see output above",
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(Error::new(
                400,
                "console_startup_failed",
                "console companion did not become ready in time",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("dg web ready: open http://{address} in a browser");
    std::io::stdout().flush().map_err(|_| Error::internal())?;
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).map_err(|_| Error::internal())?;
        let mut sigint = signal(SignalKind::interrupt()).map_err(|_| Error::internal())?;
        tokio::select! {
            status = child.wait() => {
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
                Ok(())
            }
            _ = sigint.recv() => {
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        let status = child.wait().await.map_err(|_| Error::internal())?;
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
    let (method, path, body, idempotency) =
        match cli.command {
            Command::Serve {
                listen,
                data_dir,
                backup_root,
            } => return dg::serve(listen, data_dir, backup_root).await,
            Command::Status => (Method::GET, "/v1/status".into(), None, None),
            Command::Backup {
                command: BackupCommand::Status,
            } => (Method::GET, "/v1/backups".into(), None, None),
            Command::Web { port, entry } => {
                return run_console(&cli.server, key_path, port, entry)
                    .await
                    .map_err(Application);
            }
            Command::Init { data_dir } => {
                let key = dg::store::Store::open(&data_dir)
                    .and_then(|store| store.bootstrap())
                    .map_err(Application)?;
                println!("{}", json!({"owner_key":key}));
                return Ok(());
            }
            Command::List => (Method::GET, "/v1/databases".into(), None, None),
            Command::Info { name } => {
                dg::store::validate_name(&name).map_err(Application)?;
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
                let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
                    Error::new(400, "missing_credential", "--key-file required"),
                ))?)
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
                        let bytes = base64::Engine::decode(
                            &base64::engine::general_purpose::STANDARD,
                            encoded,
                        )
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
                let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
                    Error::new(400, "missing_credential", "--key-file required"),
                ))?)
                .map_err(Application)?;
                // Chunked upload: stream the file piece by piece, then commit.
                // Only a single chunk is ever buffered in memory.
                let file = File::open(&from).map_err(|_| Application(Error::invalid()))?;
                let metadata = file.metadata().map_err(|_| Application(Error::invalid()))?;
                if !metadata.is_file() || metadata.len() > dg::store::MAX_TRANSFER_SIZE {
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
            Command::Shell => {
                let key = dg::client::read_key(key_path.as_deref().ok_or(Application(
                    Error::new(400, "missing_credential", "--key-file required"),
                ))?)
                .map_err(Application)?;
                return dg::shell::run(
                    &cli.server,
                    &key,
                    cli.json,
                    std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
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
                    dg::shell::render(&result, false, &mut std::io::stdout())
                        .map_err(Application)?;
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
                let body: dg::execution::Batch = serde_json::from_slice(&batch_bytes)
                    .map_err(|_| Application(Error::invalid()))?;
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
            } => (
                Method::POST,
                "/v1/keys".into(),
                Some(json!({"name":name,"role":role,"databases":database})),
                None,
            ),
            Command::Key {
                command: KeyCommand::List,
            } => (Method::GET, "/v1/keys".into(), None, None),
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
            tracing::error!(code=error.code(),message=%error,"command failed");
            ExitCode::FAILURE
        }
    }
}
