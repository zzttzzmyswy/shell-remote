use clap::{Parser, Subcommand};

mod agent;
mod proto;
mod relay;

#[derive(Parser)]
#[command(name = "ssh-remote", about = "Collaborative remote SSH tool")]
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

        /// Path to TLS certificate file (omit for dev mode)
        #[arg(long)]
        tls_cert: Option<String>,

        /// Path to TLS private key file (omit for dev mode)
        #[arg(long)]
        tls_key: Option<String>,

        /// Run in development mode (no TLS, plaintext WebSocket)
        #[arg(long, default_value_t = false)]
        dev: bool,
    },

    /// Run in agent mode (connects to a relay)
    Agent {
        /// WebSocket URL of the relay server
        #[arg(long, default_value = "ws://localhost:3000/ws")]
        relay_url: String,

        /// Fixed authentication key (optional, random token used if omitted)
        #[arg(long)]
        key: Option<String>,

        /// Root directory for file system sandbox (defaults to $HOME)
        #[arg(long, env = "HOME")]
        root: String,

        /// Token type: rw, ro, or both
        #[arg(long, default_value = "rw")]
        token_type: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Relay {
            bind,
            tls_cert,
            tls_key,
            dev,
        } => {
            relay::start(bind, tls_cert, tls_key, dev).await?;
        }
        Command::Agent {
            relay_url,
            key,
            root,
            token_type,
        } => {
            agent::start(relay_url, key, root, token_type).await?;
        }
    }

    Ok(())
}
