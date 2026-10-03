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
            println!("viewer: {}", viewer_line(&image).await);
            Ok(())
        }
        None => serve().await,
    }
}

const INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The viewer link of the running computer, or why there is none.
async fn viewer_line(image: &image::Image) -> String {
    let look = async {
        let docked = computer::Docked::connect(computer::Settings::from_env(), image.clone())?;
        let Some(endpoint) = docked.running_endpoint().await? else {
            return Ok("the computer is not running, call start_computer to start it".to_owned());
        };
        let Some(port) = endpoint.viewer_port else {
            return Ok("this computer predates the viewer, remove it with `docker rm` so it is created again".to_owned());
        };
        let info = client::Client::new(&endpoint)?.viewer().await?;
        Ok::<_, anyhow::Error>(computer_protocol::viewer_link(port, &info.key))
    };
    match tokio::time::timeout(INFO_TIMEOUT, look).await {
        Ok(Ok(line)) => line,
        Ok(Err(error)) => format!("unavailable ({error:#})"),
        Err(_) => "unavailable (timed out)".to_owned(),
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
