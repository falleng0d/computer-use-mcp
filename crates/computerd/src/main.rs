mod api;
mod cap;
#[cfg(target_os = "linux")]
mod exec;
#[cfg(not(target_os = "linux"))]
#[path = "exec_unsupported.rs"]
mod exec;
mod files;
mod frames;
mod guard;
mod hub;
mod key;
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "only the Linux screen types text")
)]
mod keys;
mod liveness;
mod plan;
mod screen;
mod sessions;
#[cfg(target_os = "linux")]
mod shm;
mod viewer;
mod workdir;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(not(target_os = "linux"))]
#[path = "x11_unsupported.rs"]
mod x11;

use std::{net::SocketAddr, path::PathBuf};

use anyhow::Context;
use computer_protocol::{
    API_PORT, DEFAULT_PORT_BASE, HOST_PORT_BASE_ENV, SCREEN_COUNT, TOKEN_ENV, VERSION, viewer_link,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

const NOVNC_DIR: &str = "/opt/novnc";

/// Host port the viewer page is published on, as `start_computer` passed it in.
fn host_base(value: Option<&str>) -> u16 {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PORT_BASE)
}

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
    let base = host_base(std::env::var(HOST_PORT_BASE_ENV).ok().as_deref());
    let sessions = sessions::Sessions::new(hub::Hub::new(base));
    let home = workdir::home_dir();
    let key = tokio::task::spawn_blocking(move || key::ensure(&home))
        .await
        .context("preparing the viewer password")?
        .context("preparing the viewer password")?;
    let addr = SocketAddr::from(([0, 0, 0, 0], API_PORT));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    let viewer = viewer::start(viewer::Config {
        key: key.clone(),
        sessions: sessions.clone(),
        novnc: PathBuf::from(NOVNC_DIR),
    })
    .await
    .context("starting the viewer")?;
    info!(version = VERSION, %addr, "computerd started");
    info!(
        link = %viewer_link(base, &key),
        vnc_ports = %format!("{}-{}", base + 1, base + u16::from(SCREEN_COUNT)),
        "open the computer in a browser with this link, native VNC clients use the key as the password"
    );
    let stop_reaper = CancellationToken::new();
    let reaper = {
        let sessions = sessions.clone();
        let stop = stop_reaper.clone();
        tokio::spawn(async move { sessions.reap_until(stop).await })
    };
    let stopping = sessions.clone();
    let served = axum::serve(listener, api::router(token, key, sessions.clone()))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            stopping.cancel_all();
        })
        .await
        .context("serving the API");
    stop_reaper.cancel();
    reaper.await.context("stopping the session reaper")?;
    viewer.stop().await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_base_falls_back_to_the_default_port() {
        assert_eq!(host_base(Some("21900")), 21900);
        assert_eq!(host_base(Some("junk")), DEFAULT_PORT_BASE);
        assert_eq!(host_base(None), DEFAULT_PORT_BASE);
    }
}
