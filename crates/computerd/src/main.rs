mod api;
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "only Linux runs commands")
)]
mod cap;
#[cfg(target_os = "linux")]
mod exec;
#[cfg(not(target_os = "linux"))]
#[path = "exec_unsupported.rs"]
mod exec;
mod frames;
mod guard;
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "only the Linux screen types text")
)]
mod keys;
mod plan;
mod screen;
mod sessions;
#[cfg(target_os = "linux")]
mod shm;
mod workdir;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(not(target_os = "linux"))]
#[path = "x11_unsupported.rs"]
mod x11;

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
    screen::clean_stale_x_files();
    let sessions = sessions::Sessions::default();
    let addr = SocketAddr::from(([0, 0, 0, 0], API_PORT));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    info!(version = VERSION, %addr, "computerd started");
    let stopping = sessions.clone();
    let served = axum::serve(listener, api::router(token, sessions.clone()))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            stopping.cancel_all();
        })
        .await
        .context("serving the API");
    sessions.close_all().await;
    served
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
