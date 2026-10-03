mod api;

use std::net::SocketAddr;

use anyhow::Context;
use computer_protocol::{API_PORT, TOKEN_ENV, VERSION};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty())
        .with_context(|| format!("{TOKEN_ENV} must be set"))?;
    let addr = SocketAddr::from(([0, 0, 0, 0], API_PORT));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    info!(version = VERSION, %addr, "computerd started");
    axum::serve(listener, api::router(token))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving the API")?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
