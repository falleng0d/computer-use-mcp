use axum::{Json, Router, routing::get};
use computer_protocol::{Health, PROTOCOL_VERSION, VERSION};

const LISTEN_ADDR: &str = "0.0.0.0:7070";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = Router::new().route("/health", get(health));
    let listener = tokio::net::TcpListener::bind(LISTEN_ADDR).await?;
    println!("computerd {VERSION} listening on {LISTEN_ADDR}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health() -> Json<Health> {
    Json(Health {
        protocol_version: PROTOCOL_VERSION,
        version: VERSION.to_owned(),
    })
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
