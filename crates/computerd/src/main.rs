mod api;
mod apps;
mod bridge;
mod browser;
mod cap;
mod cookie_sync;
mod cookies;
mod devtools;
mod env;
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
mod profile;
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
const OPEN_BROWSER_COMMAND: &str = "open-browser";

/// `computerd open-browser [url]`: shows the browser of the screen named by `DISPLAY`.
/// Programs on a screen run it, such as the Fluxbox menu and `xdg-open`.
async fn open_browser(url: Option<String>) -> anyhow::Result<()> {
    let display = std::env::var("DISPLAY").context("DISPLAY is not set")?;
    let screen = display
        .strip_prefix(':')
        .and_then(|rest| rest.split('.').next())
        .and_then(|number| number.parse().ok())
        .with_context(|| format!("DISPLAY={display} is not one of the computer's screens"))?;
    bridge::ask(&bridge::Request { screen, url }).await
}

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
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(OPEN_BROWSER_COMMAND) {
        return open_browser(args.next()).await;
    }
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty())
        .with_context(|| format!("{TOKEN_ENV} must be set"))?;
    screen::clean_stale_x_files();
    let base = host_base(std::env::var(HOST_PORT_BASE_ENV).ok().as_deref());
    let sessions = sessions::Sessions::new(hub::Hub::new(base));
    let home = workdir::home_dir();
    let cookie_sync = cookie_sync::install(cookie_sync::jar_path(&home)).await;
    let key = tokio::task::spawn_blocking(move || key::ensure(&home))
        .await
        .context("preparing the viewer password")?
        .context("preparing the viewer password")?;
    let launcher = tokio::net::TcpListener::bind(("127.0.0.1", bridge::PORT))
        .await
        .with_context(|| format!("listening on 127.0.0.1:{}", bridge::PORT))?;
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
    let syncer = {
        let stop = stop_reaper.clone();
        tokio::spawn(async move { cookie_sync.run(stop).await })
    };
    let bridge = tokio::spawn(bridge::serve(
        launcher,
        sessions.clone(),
        stop_reaper.clone(),
    ));
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
    bridge.await.context("stopping the browser bridge")?;
    syncer.await.context("stopping the cookie sync")?;
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
