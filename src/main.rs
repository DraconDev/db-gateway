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
    /// Start the local foundation server (liveness only; no database API yet).
    Serve {
        /// Loopback socket address. Public listening is disabled in this milestone.
        #[arg(long, default_value = "127.0.0.1:7432")]
        listen: SocketAddr,
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
        Command::Serve { listen } => dg::serve(listen).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(code = error.code(), message = %error, "startup or server failure");
            ExitCode::FAILURE
        }
    }
}
