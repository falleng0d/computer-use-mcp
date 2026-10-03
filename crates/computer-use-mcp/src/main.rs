mod client;
mod computer;
mod image;
mod observation;
mod server;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version = computer_protocol::VERSION, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the version and the computer image this build uses
    Info,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Some(Command::Info) => {
            let image = image::from_env();
            println!("version: {}", computer_protocol::VERSION);
            println!("protocol: {}", computer_protocol::PROTOCOL_VERSION);
            println!("image: {}", image.reference);
            println!("pull if missing: {}", image.pull);
            Ok(())
        }
        None => serve().await,
    }
}

async fn serve() -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    tracing::info!(version = computer_protocol::VERSION, "MCP server starting");
    let running = server::Server::from_env()
        .serve(stdio())
        .await
        .context("starting the MCP server")?;
    running.waiting().await.context("running the MCP server")?;
    Ok(())
}
