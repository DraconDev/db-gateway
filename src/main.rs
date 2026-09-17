use std::{net::SocketAddr, process::ExitCode};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dg", version, about = "DB Gateway — personal SQLite service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the owner credential locally; prints it once as JSON.
    Init {
        #[arg(long)]
        data_dir: std::path::PathBuf,
    },
    /// Start the local authenticated database server.
    Serve {
        /// Loopback socket address. Public listening is disabled in this milestone.
        #[arg(long, default_value = "127.0.0.1:7432")]
        listen: SocketAddr,
        #[arg(long, default_value = "state/dg")]
        data_dir: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();

    let result = match cli.command {
        Command::Serve { listen, data_dir } => dg::serve(listen, data_dir).await,
        Command::Init { data_dir } => match dg::store::Store::open(&data_dir).and_then(|store| store.bootstrap()) {
            Ok(key) => { println!("{}", serde_json::json!({"owner_key":key})); Ok(()) },
            Err(error) => Err(dg::ServerError::Application(error)),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(code = error.code(), message = %error, "startup or server failure");
            ExitCode::FAILURE
        }
    }
}
