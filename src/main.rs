use clap::{CommandFactory, Parser, Subcommand};
use serde_json::Value;
use std::{io::IsTerminal, net::SocketAddr, path::PathBuf, process::ExitCode};

fn api_failed(response: &Value) -> bool {
    response.get("error").is_some()
}

fn api_error() -> dg::error::Error {
    dg::error::Error::new(400, "api_error", "API request failed; see JSON response")
}

/// The text of a clap message with every ANSI escape removed.
///
/// The non-terminal fallback must be plain: it feeds pipes, redirections and
/// log capture, where escape sequences are noise that also breaks exact-text
/// matching. Stripping here keeps that true regardless of how a clap release
/// renders styling, instead of depending on `Display` happening to strip it.
fn plain(rendered: &str) -> String {
    let mut out = String::with_capacity(rendered.len());
    let mut rest = rendered;
    while let Some(start) = rest.find('\u{1b}') {
        out.push_str(&rest[..start]);
        // Skip the escape sequence: `ESC [ ... final-byte`, where the final
        // byte is the first character in the 0x40..=0x7e range.
        let after = &rest[start + 1..];
        let skipped = match after.strip_prefix('[') {
            Some(body) => body
                .char_indices()
                .find(|(_, c)| ('@'..='~').contains(c))
                .map(|(i, c)| i + c.len_utf8() + 1)
                .unwrap_or(after.len()),
            // A bare two-character escape (e.g. ESC c): drop just the escape.
            None => after.chars().next().map(char::len_utf8).unwrap_or(0),
        };
        rest = &rest[start + 1 + skipped..];
    }
    out.push_str(rest);
    out
}

/// Hand a bare `dg` off a terminal to clap, as the required-subcommand
/// build did, and never guess which stream the answer belongs on.
///
/// The subcommand is optional now, so clap no longer runs its own
/// missing-subcommand check and this has to reproduce it. Two things went
/// wrong when that was done by hand before:
///
///   * `print_help()` writes to STDOUT. The required-subcommand build wrote
///     every one of these messages to stderr, so a `dg --json | consumer`
///     pipeline that used to see silence was handed 2 KB of help prose.
///   * There are two different messages. With no argument at all clap showed
///     the full help; with any flag it printed the short "requires a
///     subcommand" line.
///
/// The command is therefore rebuilt the way clap saw it before this change
/// (a required subcommand, help-on-missing) and clap re-reads the real argv,
/// so the message and the exit code are its own. The STREAM is not left to
/// clap's error-kind table, though: that table routes
/// `DisplayHelpOnMissingArgumentOrSubcommand` differently across releases,
/// and getting this wrong is invisible in the exit code. So the rendered
/// text is written to stderr explicitly, which is the one part of the old
/// behaviour a pipeline can actually observe.
fn missing_subcommand() -> ExitCode {
    let mut required = Cli::command();
    required = required.subcommand_required(true);
    required = required.arg_required_else_help(true);
    let error = match required.try_get_matches_from_mut(std::env::args_os()) {
        Ok(_) => required.error(
            clap::error::ErrorKind::MissingSubcommand,
            "'dg' requires a subcommand but one was not provided",
        ),
        Err(error) => error,
    };
    // `error.print()` picks the stream from the error kind, so it is not
    // used: stderr is the behaviour being preserved, whatever clap's table
    // says this release. The rendered text is written here instead.
    //
    // Colour is then removed rather than left to a `Display` impl. `print()`
    // runs the text through a `Colorizer`, which strips styling when the
    // stream is not a terminal; `StyledStr`'s own `Display`/`ansi()` do not
    // promise that, and the difference is not cosmetic or visible in an exit
    // code -- it changes the byte count, splits words like "Usage:" so a
    // plain-text search stops matching, and puts escape sequences in front of
    // a pipeline. So the escapes are stripped here, unconditionally: this
    // path only ever runs when the output is NOT going to a terminal.
    use std::io::Write;
    let rendered = error.render().to_string();
    let text = plain(&rendered);
    let mut stderr = std::io::stderr();
    // clap's rendered text already ends with a newline; only add one when it
    // does not, so the output matches the required-subcommand build byte for
    // byte rather than gaining a blank line.
    let _ = stderr.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = stderr.write_all(b"\n");
    }
    ExitCode::from(2)
}

/// The theme for a command that is drawing to the terminal.
fn terminal_theme() -> dg::render::Theme {
    dg::render::Theme::for_stream(std::io::stdout().is_terminal())
}

/// Show an API error envelope.
///
/// A terminal gets the same one-line error the shell draws; a pipe
/// gets the JSON envelope, because that is what a machine reads. The
/// error is still an error either way: the command exits nonzero.
fn show_error(result: &Value, json: bool) {
    if json || !std::io::stdout().is_terminal() {
        println!("{result}");
        return;
    }
    let _ = dg::shell::render_with(result, false, &terminal_theme(), &mut std::io::stdout());
}

/// How a command draws its result for a person.
///
/// It owns what it needs: the view outlives the call, so it cannot
/// borrow the command's arguments.
type View = Box<dyn Fn(&Value) -> dg::render::Outcome>;

/// What a command needs to make a request and show its result.
struct Call<'a> {
    server: &'a str,
    json: bool,
    key_path: &'a Option<PathBuf>,
    method: reqwest::Method,
    path: String,
    body: Option<Value>,
    idempotency: Option<String>,
    /// What the request can commit.
    ///
    /// The method cannot say it: a batch is a POST whether it reads or
    /// writes. The statements decide, with the same rule the server
    /// uses, so the human view and the JSON view of one command say
    /// the same thing about a failure.
    semantics: dg::client::Semantics,
}

/// Fetch a result and show it to a person, or hand back the JSON when
/// the output is a machine's.
///
/// This is the ONE rule every command follows: a terminal gets the
/// human view, a pipe or `--json` gets the JSON. `view` is what the
/// command's result looks like to a person; it is only consulted for a
/// terminal, so the machine surface never changes because of it.
///
/// Returns `Ok(Some(result))` when it has drawn the view, and
/// `Ok(None)` when the output is a machine's and the caller must
/// still make the request. The request is made exactly once either
/// way: a command that draws the view must not also hand the request
/// back for the caller to run again.
async fn human_result(call: Call<'_>, view: View) -> Result<Option<Value>, dg::ServerError> {
    use dg::ServerError::Application;
    if call.json || !std::io::stdout().is_terminal() {
        return Ok(None);
    }
    let key = dg::client::read_key(call.key_path.as_deref().ok_or_else(|| {
        Application(dg::error::Error::new(
            400,
            "missing_credential",
            "--key-file required",
        ))
    })?)
    .map_err(Application)?;
    let result = dg::client::request_with_semantics(
        call.server,
        &key,
        call.method,
        &call.path,
        call.body,
        call.idempotency.as_deref(),
        call.semantics,
    )
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
    Ok(Some(result))
}

/// Fetch a JSON document from the gateway.
///
/// A command that needs the response in BOTH forms uses this: it shapes
/// the answer itself, so a person and a script see the same thing.
async fn fetch(
    server: &str,
    key_path: &Option<PathBuf>,
    method: reqwest::Method,
    path: &str,
) -> Result<Value, dg::ServerError> {
    use dg::ServerError::Application;
    let key = dg::client::read_key(key_path.as_deref().ok_or_else(|| {
        Application(dg::error::Error::new(
            400,
            "missing_credential",
            "--key-file required",
        ))
    })?)
    .map_err(Application)?;
    dg::client::request(server, &key, method, path, None, None)
        .await
        .map_err(Application)
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
#[command(
    name = "dg",
    version,
    about = "DB Gateway — personal SQLite service",
    // The bare-invocation behaviour is part of the command surface: a person
    // who types `dg` gets the operator's screen, so the help has to say so
    // instead of leaving `[COMMAND]` looking like a half-typed command.
    after_help = "Running `dg` with no COMMAND opens the terminal view on a terminal; \
                  a pipe or --json prints this help instead. The view needs a credential, \
                  like every other command — see `dg tui`."
)]
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
    /// No command opens the terminal view (see `dg tui`).
    ///
    /// Optional, not required: `dg` on its own is the operator's screen.
    /// A pipe or `--json` gets the usage text instead, so nothing that
    /// parses `dg`'s output can start a full-screen view by accident.
    #[command(subcommand)]
    command: Option<Command>,
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
        /// Keep running when the process that started this server exits.
        ///
        /// By default `dg serve` asks the kernel to shut it down when its
        /// launcher dies, so a test, audit or agent run that is killed
        /// cannot leave a server (and its data directory and port) behind
        /// for days. Set this when the server is genuinely meant to outlive
        /// its launcher -- a deliberate `nohup dg serve &` followed by
        /// closing the shell -- which no documented workflow needs, since
        /// the supported supervisor is the systemd unit.
        #[arg(long)]
        detach_safe: bool,
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
    ///
    /// The batch API accepts 32 statements at a time, and the whole
    /// file is one atomic batch, so a file of more than 32 statements
    /// is refused before it is sent: the command reports how many
    /// statements the file holds, what the API accepts, and that
    /// nothing was sent. A longer file cannot be run by splitting it
    /// into several batches, because that gives up the atomicity a
    /// migration needs.
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
    ///
    /// The batch API accepts 32 statements at a time, so a batch file
    /// of more than 32 statements is refused before it is sent.
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
    /// Open the terminal operator view.
    ///
    /// Four tabs — databases, service, backups and keys — reached with
    /// no particular key at all, each given the
    /// whole body of the screen so a wide table is not squeezed into a
    /// fraction of the width. The strip names every tab whether or not
    /// it is showing, with its count where it has one, so nothing is
    /// hidden just because it is closed.
    ///
    /// The view acts on what it shows: create, delete, clone, export
    /// and import a database, issue, revoke and rotate a key, and run a
    /// query, a SQL file or a batch. The facts come from the same view
    /// layer the one-shot commands draw from, and the requests are the
    /// API's own, so the two surfaces cannot disagree about the same
    /// gateway.
    Tui {
        /// How often the service is read again, in seconds.
        #[arg(long, default_value_t = 2)]
        refresh: u64,
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
mod cmd_backup;
mod cmd_data;
mod cmd_keys;
mod cmd_lifecycle;
mod cmd_serve;
mod cmd_status;

/// A request the shared tail in `run` issues on behalf of a command
/// handler: (method, path, body, idempotency key). A handler returns
/// `None` when it fully handled the command itself (the serve/init/
/// shell/export/import and offline backup commands, plus any command
/// whose table rendering already answered the operator).
/// A request the command built: the method, the path, the body, the
/// idempotency key, and the transport semantics.
///
/// The semantics are the request's own, not the method's: a query is a
/// POST either way, and a read that is given up on must not be reported
/// as a write that may have committed.
type Request = (
    reqwest::Method,
    String,
    Option<Value>,
    Option<String>,
    dg::client::Semantics,
);

async fn run(cli: Cli) -> Result<(), dg::ServerError> {
    use dg::{ServerError::Application, error::Error};
    // Credential resolution: explicit flag, env, then the default key file.
    // Missing everywhere is the same local error as before.
    let key_path = cli.key_file.clone().or_else(dg::client::default_key_file);
    let request = match cli.command.expect("a command is resolved before run") {
        Command::Serve {
            listen,
            data_dir,
            backup_root,
            backup_digest_secs,
            backup_failure_poll_secs,
            backup_notify_webhook,
            detach_safe,
        } => {
            return cmd_serve::serve(
                listen,
                data_dir,
                backup_root,
                backup_digest_secs,
                backup_failure_poll_secs,
                backup_notify_webhook,
                detach_safe,
            )
            .await;
        }
        Command::Init { data_dir } => return cmd_serve::init(data_dir, cli.json).await,
        Command::Shell { name } => {
            return cmd_data::shell(&cli.server, cli.json, &key_path, name).await;
        }
        Command::Tui { refresh } => {
            // The view takes the terminal over, so it needs one.
            if !std::io::stdout().is_terminal() {
                return Err(Application(Error::new(
                    400,
                    "not_a_terminal",
                    "the terminal view needs a terminal; use the one-shot commands for machine output",
                )));
            }
            let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
                400,
                "missing_credential",
                "--key-file required",
            )))?)
            .map_err(Application)?;
            return dg::tui::run(
                &cli.server,
                &key,
                dg::tui::Options {
                    refresh_secs: refresh,
                },
            )
            .await
            .map_err(Application);
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
        } => cmd_lifecycle::clone_db(&cli.server, cli.json, &key_path, source, destination).await?,
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
        } => {
            cmd_data::batch(
                &cli.server,
                cli.json,
                &key_path,
                name,
                file,
                idempotency_key,
            )
            .await?
        }
        Command::Key {
            command:
                KeyCommand::Create {
                    name,
                    role,
                    database,
                },
        } => cmd_keys::create(&cli.server, cli.json, &key_path, name, role, database).await?,
        Command::Key {
            command: KeyCommand::List,
        } => cmd_keys::list(&cli.server, cli.json, &key_path).await?,
        Command::Key {
            command: KeyCommand::Tokens { database },
        } => cmd_keys::tokens(&cli.server, cli.json, &key_path, database).await?,
        Command::Key {
            command: KeyCommand::Revoke { name },
        } => cmd_keys::revoke(&cli.server, cli.json, &key_path, name).await?,
        Command::Key {
            command: KeyCommand::RotateOwner,
        } => cmd_keys::rotate_owner(&cli.server, cli.json, &key_path).await?,
    };
    let Some((method, path, body, idempotency, semantics)) = request else {
        return Ok(());
    };
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    let result = dg::client::request_with_semantics(
        &cli.server,
        &key,
        method,
        &path,
        body,
        idempotency.as_deref(),
        semantics,
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
    let mut cli = Cli::parse();
    // `dg` on its own is the operator's screen, not a usage dump. The view
    // takes the whole terminal over, so it is only chosen where there is a
    // terminal to take over: a pipe, a `--json` caller and a redirected
    // stdin all keep the old behaviour, because a script that runs `dg` to
    // read its command list must not get a full-screen view it cannot drive.
    if cli.command.is_none() {
        let interactive =
            !cli.json && std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
        if interactive {
            cli.command = Some(Command::Tui { refresh: 2 });
        } else {
            // Not a terminal: this has to be the SAME output the required-
            // subcommand binary produced, which is why it is not rebuilt by
            // hand. Making the subcommand optional removed clap's own
            // "missing subcommand" check, and a hand-rolled `print_help()`
            // wrote the help to STDOUT -- where the old build wrote nothing
            // but JSON consumers were then handed 2 KB of prose. So the
            // command is put back the way clap saw it before this change and
            // clap produces its own error: full help on stderr when no
            // argument was given, the short "requires a subcommand" line on
            // stderr when flags were, and exit 2 either way.
            return missing_subcommand();
        }
    }
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
