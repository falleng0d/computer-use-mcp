//! What the user connects to: the viewer page, its event stream, the WebSocket bridge for noVNC,
//! and one raw VNC port per screen. Every viewer goes through here, so `computerd` can count them.

use std::{
    convert::Infallible,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        Path as UrlPath, Query, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use computer_protocol::{SCREEN_COUNT, VIEWER_PORT, vnc_port};
use futures_util::Stream;
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{broadcast, watch},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{debug, warn};

use crate::{
    hub::{Hub, PageGuard, ViewerGuard},
    screen::XVNC_FIRST_PORT,
    sessions::Sessions,
};

const KEY_HEADER: &str = "x-viewer-key";
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const XVNC_HOST: &str = "127.0.0.1";
const COPY_BUFFER: usize = 32 * 1024;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// Port of the `Xvnc` that serves `screen`, reachable only from inside the container.
fn xvnc_port(screen: u8) -> u16 {
    XVNC_FIRST_PORT + u16::from(screen)
}

/// True when `host` is a `Host` header value that can only come from a page opened on this machine.
fn host_allowed(host: &str, port: u16) -> bool {
    host.rsplit_once(':').is_some_and(|(name, given)| {
        given.parse() == Ok(port) && (name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost"))
    })
}

/// True when `origin` is a page served by this viewer.
fn origin_allowed(origin: &str, port: u16) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|host| host_allowed(host, port))
}

/// Maps a URL path below `/novnc/` to a file below `root`, or `None` when it could leave `root`.
fn static_path(root: &Path, rest: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in rest.split('/') {
        let plain = !part.is_empty()
            && !part.starts_with('.')
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !plain {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn keys_match(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Everything the viewer routes share.
#[derive(Clone)]
struct ViewerState {
    key: Arc<str>,
    host_port: u16,
    sessions: Sessions,
    hub: Hub,
    novnc: Arc<Path>,
    stop: CancellationToken,
}

impl ViewerState {
    fn key_ok(&self, headers: &HeaderMap, query: &KeyQuery) -> bool {
        let given = headers
            .get(KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .or(query.key.as_deref());
        given.is_some_and(|given| keys_match(given.as_bytes(), self.key.as_bytes()))
    }
}

#[derive(Deserialize)]
struct KeyQuery {
    key: Option<String>,
}

fn router(state: ViewerState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/novnc/{*path}", get(novnc_file))
        .route("/events", get(events))
        .route("/ws/{screen}", get(bridge))
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .with_state(state)
}

/// Refuses requests that a page on another site could make through DNS rebinding or a cross-site WebSocket.
async fn check_origin(State(state): State<ViewerState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let text = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let host_ok = text(header::HOST).is_some_and(|host| host_allowed(host, state.host_port));
    let origin_ok = headers.get(header::ORIGIN).is_none()
        || text(header::ORIGIN).is_some_and(|origin| origin_allowed(origin, state.host_port));
    if host_ok && origin_ok {
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

fn page(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    page("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    page("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> Response {
    page("text/css; charset=utf-8", APP_CSS)
}

async fn novnc_file(State(state): State<ViewerState>, UrlPath(rest): UrlPath<String>) -> Response {
    let Some(path) = static_path(&state.novnc, &rest) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(content_type(&path)),
                ),
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// State of one open page's event stream.
struct Feed {
    sessions: Sessions,
    changes: watch::Receiver<u64>,
    shown: broadcast::Receiver<u8>,
    stop: CancellationToken,
    first: bool,
    _page: PageGuard,
}

impl Feed {
    fn snapshot(&self) -> Event {
        Event::default()
            .event("sessions")
            .json_data(self.sessions.views())
            .expect("session views always serialize")
    }

    /// Waits for the next event for the page, or `None` when the stream is over.
    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        if std::mem::take(&mut self.first) {
            let event = self.snapshot();
            return Some((Ok(event), self));
        }
        loop {
            let event = tokio::select! {
                () = self.stop.cancelled() => return None,
                changed = self.changes.changed() => {
                    changed.ok()?;
                    self.snapshot()
                }
                shown = self.shown.recv() => match shown {
                    Ok(screen) => Event::default().event("show").data(screen.to_string()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
            };
            return Some((Ok(event), self));
        }
    }
}

async fn events(
    State(state): State<ViewerState>,
    headers: HeaderMap,
    Query(query): Query<KeyQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if !state.key_ok(&headers, &query) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let feed = Feed {
        sessions: state.sessions.clone(),
        changes: state.hub.subscribe_changes(),
        shown: state.hub.subscribe_show(),
        stop: state.stop.clone(),
        first: true,
        _page: state.hub.attach_page(),
    };
    Ok(Sse::new(futures_util::stream::unfold(feed, Feed::next)).keep_alive(KeepAlive::default()))
}

fn valid_screen(screen: u8) -> bool {
    (1..=SCREEN_COUNT).contains(&screen)
}

/// Opens the connection to the `Xvnc` of `screen`.
async fn connect_screen(screen: u8) -> io::Result<TcpStream> {
    let address = (XVNC_HOST, xvnc_port(screen));
    tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

async fn bridge(
    State(state): State<ViewerState>,
    UrlPath(screen): UrlPath<u8>,
    headers: HeaderMap,
    Query(query): Query<KeyQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !state.key_ok(&headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !valid_screen(screen) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(upstream) = connect_screen(screen).await else {
        return (StatusCode::NOT_FOUND, "this screen is not open").into_response();
    };
    let viewer = state.hub.attach(screen);
    let stop = state.stop.clone();
    upgrade.on_upgrade(move |socket| relay_ws(socket, upstream, viewer, stop))
}

/// Copies bytes between a noVNC WebSocket and the screen's VNC connection until either side ends.
async fn relay_ws(
    mut socket: WebSocket,
    mut upstream: TcpStream,
    _viewer: ViewerGuard,
    stop: CancellationToken,
) {
    let (mut from_vnc, mut to_vnc) = upstream.split();
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(data))) => {
                    if to_vnc.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            read = from_vnc.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if socket.send(Message::Binary(buffer[..count].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            },
        }
    }
}

/// Forwards one native VNC client to the screen's `Xvnc` until either side ends.
async fn relay_tcp(mut client: TcpStream, screen: u8, hub: Hub, stop: CancellationToken) {
    let Ok(mut upstream) = connect_screen(screen).await else {
        debug!(screen, "refused a VNC client, the screen is not open");
        return;
    };
    let _viewer = hub.attach(screen);
    tokio::select! {
        () = stop.cancelled() => {}
        _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
    }
}

async fn accept_vnc(listener: TcpListener, screen: u8, hub: Hub, stop: CancellationToken) {
    let connections = TaskTracker::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((client, _)) => {
                    drop(connections.spawn(relay_tcp(client, screen, hub.clone(), stop.clone())));
                }
                Err(error) => {
                    warn!(screen, %error, "accepting a VNC client failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    connections.close();
    connections.wait().await;
}

/// What the viewer needs from the rest of the daemon.
pub struct Config {
    pub key: String,
    pub sessions: Sessions,
    pub novnc: PathBuf,
}

/// The running viewer servers.
pub struct Running {
    tasks: TaskTracker,
    stop: CancellationToken,
}

impl Running {
    /// Closes every viewer connection and waits for the servers to end.
    pub async fn stop(self) {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// Binds the page port and the 16 raw VNC ports, then serves them in tasks that `Running` owns.
pub async fn start(config: Config) -> Result<Running> {
    let hub = config.sessions.hub();
    let stop = CancellationToken::new();
    let tasks = TaskTracker::new();
    let bind = async |port: u16| {
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        TcpListener::bind(address)
            .await
            .with_context(|| format!("listening on {address}"))
    };

    let page = bind(VIEWER_PORT).await?;
    let state = ViewerState {
        key: config.key.into(),
        host_port: hub.host_base(),
        sessions: config.sessions,
        hub: hub.clone(),
        novnc: config.novnc.into(),
        stop: stop.clone(),
    };
    let shutdown = stop.clone();
    drop(tasks.spawn(async move {
        let served = axum::serve(page, router(state))
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        if let Err(error) = served {
            warn!(%error, "the viewer page server stopped");
        }
    }));
    for screen in 1..=SCREEN_COUNT {
        let listener = bind(vnc_port(screen)).await?;
        drop(tasks.spawn(accept_vnc(listener, screen, hub.clone(), stop.clone())));
    }
    Ok(Running { tasks, stop })
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn only_loopback_names_on_the_published_port_are_accepted() {
        assert!(host_allowed("127.0.0.1:20900", 20900));
        assert!(host_allowed("localhost:21900", 21900));
        assert!(host_allowed("LocalHost:20900", 20900));
        assert!(!host_allowed("127.0.0.1:20901", 20900));
        assert!(!host_allowed("127.0.0.1", 20900));
        assert!(!host_allowed("evil.example:20900", 20900));
        assert!(!host_allowed("127.0.0.1.evil.example:20900", 20900));
        assert!(!host_allowed("evil.localhost.example:20900", 20900));
        assert!(origin_allowed("http://127.0.0.1:20900", 20900));
        assert!(origin_allowed("http://localhost:20900", 20900));
        assert!(!origin_allowed("null", 20900));
        assert!(!origin_allowed("https://127.0.0.1:20900", 20900));
        assert!(!origin_allowed("http://evil.example:20900", 20900));
    }

    #[test]
    fn static_files_cannot_escape_the_novnc_folder() {
        let root = Path::new("/opt/novnc");
        assert_eq!(
            static_path(root, "core/rfb.js"),
            Some(root.join("core").join("rfb.js"))
        );
        for bad in [
            "../etc/passwd",
            "core/../../x",
            "core//rfb.js",
            ".env",
            "a\\b",
            "",
        ] {
            assert_eq!(static_path(root, bad), None, "{bad}");
        }
    }

    fn app(root: &Path) -> (Router, Hub) {
        let sessions = Sessions::default();
        let hub = sessions.hub();
        let state = ViewerState {
            key: "abcd2345".into(),
            host_port: 20900,
            sessions,
            hub: hub.clone(),
            novnc: root.into(),
            stop: CancellationToken::new(),
        };
        (router(state), hub)
    }

    fn get(uri: &str, host: &str, key: Option<&str>) -> axum::http::Request<axum::body::Body> {
        let mut request = axum::http::Request::builder()
            .uri(uri)
            .header(header::HOST, host);
        if let Some(key) = key {
            request = request.header(KEY_HEADER, key);
        }
        request.body(axum::body::Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn foreign_hosts_and_origins_are_refused_before_any_route_runs() {
        let root = std::env::temp_dir();
        let (app, _) = app(&root);
        let status = |request: axum::http::Request<axum::body::Body>| {
            let app = app.clone();
            async move { app.oneshot(request).await.unwrap().status() }
        };
        assert_eq!(
            status(get("/", "127.0.0.1:20900", None)).await,
            StatusCode::OK
        );
        assert_eq!(
            status(get("/", "rebind.example:20900", None)).await,
            StatusCode::FORBIDDEN
        );
        let cross_site = axum::http::Request::builder()
            .uri("/")
            .header(header::HOST, "127.0.0.1:20900")
            .header(header::ORIGIN, "http://evil.example")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status(cross_site).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_event_stream_needs_the_key_and_counts_the_open_page() {
        use futures_util::StreamExt;

        let root = std::env::temp_dir();
        let (app, hub) = app(&root);
        let wrong = app
            .clone()
            .oneshot(get("/events", "127.0.0.1:20900", Some("abcd2346")))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(hub.pages(), 0);

        let response = app
            .oneshot(get("/events", "127.0.0.1:20900", Some("abcd2345")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        assert_eq!(
            &first[..],
            b"event: sessions
data: []

"
        );
        assert_eq!(hub.pages(), 1);

        assert_eq!(hub.show(2), 1);
        let shown = body.next().await.unwrap().unwrap();
        assert_eq!(
            &shown[..],
            b"event: show
data: 2

"
        );
        drop(body);
        assert_eq!(hub.pages(), 0);
    }

    #[test]
    fn the_key_must_match_in_full() {
        assert!(keys_match(b"abcd2345", b"abcd2345"));
        assert!(!keys_match(b"abcd2346", b"abcd2345"));
        assert!(!keys_match(b"abcd234", b"abcd2345"));
        assert!(!keys_match(b"", b"abcd2345"));
    }
}
