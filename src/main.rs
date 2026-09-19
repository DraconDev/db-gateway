use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{io::IsTerminal, net::SocketAddr, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "dg", version, about = "DB Gateway — personal SQLite service")]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:7432")]
    server: String,
    /// File containing a raw credential (0600 on Unix). Never pass secrets as flags.
    #[arg(long, global = true)]
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
    /// Run the authenticated server (loopback only in M1).
    Serve {
        #[arg(long, default_value = "127.0.0.1:7432")]
        listen: SocketAddr,
        #[arg(long, default_value = "state/dg")]
        data_dir: PathBuf,
    },
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
    /// Import a local .sqlite file as a new database (owner/app key).
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
}
#[derive(Subcommand)]
enum KeyCommand {
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
}
async fn run(cli: Cli) -> Result<(), dg::ServerError> {
    use dg::{ServerError::Application, error::Error};
    use reqwest::Method;
    let (method, path, body, idempotency) = match cli.command {
        Command::Serve { listen, data_dir } => return dg::serve(listen, data_dir).await,
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
            let key = dg::client::read_key(cli.key_file.as_deref().ok_or(Application(
                Error::new(400, "missing_credential", "--key-file required"),
            ))?)
            .map_err(Application)?;
            let response = dg::client::request(
                &cli.server,
                &key,
                Method::POST,
                &format!("/v1/databases/{name}/export"),
                Some(json!({})),
                None,
            )
            .await
            .map_err(Application)?;
            if response.get("error").is_some() {
                println!("{response}");
                return Err(Application(Error::new(
                    400,
                    "api_error",
                    "export failed; see JSON response",
                )));
            }
            let encoded = response["data_base64"]
                .as_str()
                .ok_or(Application(Error::internal()))?;
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                .map_err(|_| Application(Error::internal()))?;
            std::fs::write(&out, bytes).map_err(|_| Application(Error::internal()))?;
            println!(
                "{}",
                json!({"name":name,"out":out.display().to_string(),"size":response["size"],"sha256":response["sha256"]})
            );
            return Ok(());
        }
        Command::Import { name, from } => {
            dg::store::validate_name(&name).map_err(Application)?;
            let bytes = std::fs::read(&from).map_err(|_| Application(Error::invalid()))?;
            let encoded =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
            (
                Method::POST,
                format!("/v1/databases/{name}/import"),
                Some(json!({"name":name,"data_base64":encoded})),
                None,
            )
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
            let key = dg::client::read_key(cli.key_file.as_deref().ok_or(Application(
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
            if std::fs::metadata(&file)
                .map_err(|_| Application(Error::invalid()))?
                .len()
                > 65536
            {
                return Err(Application(Error::invalid()));
            }
            let body: dg::execution::Batch = serde_json::from_slice(
                &std::fs::read(file).map_err(|_| Application(Error::invalid()))?,
            )
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
            command: KeyCommand::Revoke { name },
        } => {
            dg::store::validate_name(&name).map_err(Application)?;
            (Method::DELETE, format!("/v1/keys/{name}"), None, None)
        }
    };
    let key = dg::client::read_key(cli.key_file.as_deref().ok_or(Application(Error::new(
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
