use crate::proto::TokenType;
use clap::{Parser, Subcommand};

mod agent;
#[cfg(test)]
mod integration_test;
mod proto;
mod relay;
mod web;

#[derive(Parser)]
#[command(name = "shell-remote", about = "Collaborative remote shell tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run in relay (server) mode
    Relay {
        /// Address to bind the relay server
        #[arg(long, default_value = "0.0.0.0:3000")]
        bind: String,

        /// Server access password (required)
        #[arg(long)]
        auth: Option<String>,
    },

    /// Run in agent mode (connects to a relay)
    Agent {
        /// WebSocket URL of the relay server
        #[arg(long, default_value = "ws://localhost:3000")]
        relay_url: String,

        /// Fixed authentication key (optional, random token used if omitted)
        #[arg(long)]
        key: Option<String>,

        /// Default directory for file manager (defaults to $HOME)
        #[arg(long, env = "HOME")]
        root: String,

        /// Token type: rw, ro, or both
        #[arg(long, default_value = "rw")]
        token_type: TokenType,

        /// Shell path (e.g., /bin/bash, /usr/bin/zsh)
        #[arg(long, env = "SHELL", default_value = "/bin/bash")]
        shell: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let version = env!("CARGO_PKG_VERSION");
    tracing::info!("shell-remote v{}", version);

    match cli.command {
        Command::Relay { bind, auth } => {
            relay::start(bind, auth).await?;
        }
        Command::Agent {
            relay_url,
            key,
            root,
            token_type,
            shell,
        } => {
            agent::start(relay_url, key, root, token_type.as_str().to_string(), shell).await?;
        }
    }

    Ok(())
}
