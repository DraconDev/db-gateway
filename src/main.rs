use clap::{Parser, Subcommand};
use serde_json::Value;
use std::{
    io::{IsTerminal, Write},
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

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> dg::render::Theme {
    dg::render::Theme::for_stream(std::io::stdout().is_terminal())
}

/// How a command draws its result for a person.
///
/// It owns what it needs: the view outlives the call, so it cannot
/// borrow the command's arguments.
type View = Box<dyn Fn(&Value) -> dg::render::Outcome>;

/// Fetch a result and show it to a person, or hand back the JSON when
/// the output is a machine's.
///
/// This is the ONE rule every command follows: a terminal gets the
/// human view, a pipe or `--json` gets the JSON. `view` is what the
/// command's result looks like to a person; it is only consulted for a
/// terminal, so the machine surface never changes because of it.
async fn human_result(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    idempotency: Option<&str>,
    view: View,
) -> Result<Option<Value>, dg::ServerError> {
    use dg::ServerError::Application;
    if json || !std::io::stdout().is_terminal() {
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
    let result = dg::client::request(server, &key, method, path, body, idempotency)
        .await
        .map_err(Application)?;
    if api_failed(&result) {
        dg::shell::render_with(&result, false, &terminal_theme(), &mut std::io::stdout())
            .map_err(Application)?;
        return Err(Application(api_error()));
    }
    let mut out = std::io::stdout();
    view(&result)
        .write(&mut out, &terminal_theme())
        .map_err(Application)?;
    Ok(None)
}

/// Terminal-table fetch shared by the JSON-listing commands. Returns
/// `Ok(None)` when output stays machine JSON (piped stdout or --json);
/// otherwise fetches, renders any error envelope, and hands the JSON
/// back for table shaping (Err on API failure).
async fn table_json(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    method: reqwest::Method,
    path: &str,
) -> Result<Option<Value>, dg::ServerError> {
    use dg::ServerError::Application;
    if json || !std::io::stdout().is_terminal() {
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
    let result = dg::client::request(server, &key, method, path, None, None)
        .await
        .map_err(Application)?;
    if api_failed(&result) {
        dg::shell::render_with(
            &result,
            false,
            &dg::render::Theme::for_stream(std::io::stdout().is_terminal()),
            &mut std::io::stdout(),
        )
        .map_err(Application)?;
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
    /// A one-shot snapshot of the service dashboard.
    ///
    /// The same view as `dg status`, read once and printed. It exits
    /// rather than refreshing in place, so it stays pipeable and
    /// diffable: `dg watch > before.json` then `dg watch > after.json`
    /// compares two moments.
    Watch,
    /// List database names visible to the credential (owner sees all).
    List,
    /// Inspect one database: what it holds and what it has been doing.
    ///
    /// `dg info` is what a database is; `dg inspect` is what it has been
    /// doing -- its traffic, its queue waits, its last write, and the
    /// freshness of its backup.
    Inspect {
        /// Database to inspect.
        name: String,
    },
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
    /// Run a SQL file against a database, reporting each statement.
    ///
    /// The file is split into statements by SQLite itself, so a
    /// semicolon inside a string or a trigger body does not end a
    /// statement. The statements run as one atomic batch: either every
    /// statement lands or none does.
    Execute {
        /// Database to run the script against.
        name: String,
        /// The SQL file to run.
        file: PathBuf,
        /// Replay key: a retry with the same key and identical body returns
        /// the stored first response instead of re-running the script.
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
    /// List the credentials that can act on one database.
    ///
    /// The owner key reaches every database; an application key reaches
    /// the databases it was scoped to. This is the first question of a
    /// leaked-key incident.
    Tokens {
        /// Database to list the credentials for.
        database: String,
    },
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
mod cmd_backup;
mod cmd_data;
mod cmd_keys;
mod cmd_lifecycle;
mod cmd_serve;
mod cmd_status;
mod console;

/// A request the shared tail in `run` issues on behalf of a command
/// handler: (method, path, body, idempotency key). A handler returns
/// `None` when it fully handled the command itself (the serve/init/
/// console/shell/export/import and offline backup commands, plus any
/// command whose table rendering already answered the operator).
type Request = (reqwest::Method, String, Option<Value>, Option<String>);

async fn run(cli: Cli) -> Result<(), dg::ServerError> {
    use dg::{ServerError::Application, error::Error};
    // Credential resolution: explicit flag, env, then the default key file.
    // Missing everywhere is the same local error as before.
    let key_path = cli.key_file.clone().or_else(dg::client::default_key_file);
    let request = match cli.command {
        Command::Serve {
            listen,
            data_dir,
            backup_root,
            backup_digest_secs,
            backup_failure_poll_secs,
            backup_notify_webhook,
        } => {
            return cmd_serve::serve(
                listen,
                data_dir,
                backup_root,
                backup_digest_secs,
                backup_failure_poll_secs,
                backup_notify_webhook,
            )
            .await;
        }
        Command::Init { data_dir } => return cmd_serve::init(data_dir, cli.json).await,
        Command::Web {
            port,
            entry,
            data_dir,
        } => {
            return console::run_console(&cli.server, key_path, port, entry, data_dir)
                .await
                .map_err(Application);
        }
        Command::Shell { name } => {
            return cmd_data::shell(&cli.server, cli.json, &key_path, name).await;
        }
        Command::Export { name, out } => {
            return cmd_lifecycle::export(&cli.server, cli.json, &key_path, name, out).await;
        }
        Command::Import { name, from } => {
            return cmd_lifecycle::import(&cli.server, cli.json, &key_path, name, from).await;
        }
        Command::Backup {
            command:
                BackupCommand::Digest {
                    data_dir,
                    backup_root,
                    json: as_json,
                },
        } => return cmd_backup::digest(data_dir, backup_root, as_json).await,
        Command::Backup {
            command: BackupCommand::Verify { backup_root, set },
        } => return cmd_backup::verify(backup_root, set, cli.json).await,
        Command::Status => cmd_status::status(&cli.server, cli.json, &key_path).await?,
        Command::Watch => cmd_status::watch(&cli.server, cli.json, &key_path).await?,
        Command::Backup {
            command: BackupCommand::Status,
        } => cmd_backup::status(&cli.server, cli.json, &key_path).await?,
        Command::List => cmd_data::list(&cli.server, cli.json, &key_path).await?,
        Command::Inspect { name } => {
            cmd_data::inspect(&cli.server, cli.json, &key_path, name).await?
        }
        Command::Info { name } => cmd_data::info(&cli.server, cli.json, &key_path, name).await?,
        Command::Create { name } => {
            cmd_data::create(&cli.server, cli.json, &key_path, name).await?
        }
        Command::Clone {
            source,
            destination,
        } => {
            cmd_lifecycle::clone_db(&cli.server, cli.json, &key_path, source, destination).await?
        }
        Command::Delete { name, confirm } => {
            cmd_lifecycle::delete(&cli.server, cli.json, &key_path, name, confirm).await?
        }
        Command::Query {
            name,
            sql,
            args,
            idempotency_key,
        } => {
            cmd_data::query(
                &cli.server,
                cli.json,
                &key_path,
                name,
                sql,
                args,
                idempotency_key,
            )
            .await?
        }
        Command::Execute {
            name,
            file,
            idempotency_key,
        } => {
            cmd_data::execute(
                &cli.server,
                cli.json,
                &key_path,
                name,
                file,
                idempotency_key,
            )
            .await?
        }
        Command::Batch {
            name,
            file,
            idempotency_key,
        } => cmd_data::batch(name, file, idempotency_key).await?,
        Command::Key {
            command:
                KeyCommand::Create {
                    name,
                    role,
                    database,
                },
        } => {
            cmd_keys::create(&cli.server, cli.json, &key_path, name, role, database).await?
        }
        Command::Key {
            command: KeyCommand::List,
        } => cmd_keys::list(&cli.server, cli.json, &key_path).await?,
        Command::Key {
            command: KeyCommand::Tokens { database },
        } => cmd_keys::tokens(&cli.server, cli.json, &key_path, database).await?,
        Command::Key {
            command: KeyCommand::Revoke { name },
        } => {
            cmd_keys::revoke(&cli.server, cli.json, &key_path, name).await?
        }
        Command::Key {
            command: KeyCommand::RotateOwner,
        } => {
            cmd_keys::rotate_owner(&cli.server, cli.json, &key_path).await?
        }
    };
    let Some((method, path, body, idempotency)) = request else {
        return Ok(());
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
