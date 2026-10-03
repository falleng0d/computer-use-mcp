mod client;
mod computer;
mod file_result;
mod image;
mod observation;
mod server;
mod shell_result;

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
    let server = server::Server::from_env();
    let running = server
        .clone()
        .serve(stdio())
        .await
        .context("starting the MCP server")?;
    let stop = running.cancellation_token();
    let waited = tokio::select! {
        result = running.waiting() => result.map(|_| ()).context("running the MCP server"),
        () = shutdown_signal() => {
            stop.cancel();
            Ok(())
        }
    };
    server.shutdown().await;
    waited
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let (Ok(mut terminate), Ok(mut hangup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
        _ = hangup.recv() => {}
    }
}

#[cfg(windows)]
async fn shutdown_signal() {
    let Ok(mut close) = tokio::signal::windows::ctrl_close() else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = close.recv() => {}
    }
}

#[cfg(not(any(unix, windows)))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
