//! What the user connects to: the viewer page, its event stream, the WebSocket bridge for noVNC,
//! and one raw VNC port per screen. Every viewer goes through here, so `computerd` can count them.

use std::{
    convert::Infallible,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll},
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
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch},
    time::Instant,
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
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const REFUSAL_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_VNC_CONNECTIONS_PER_SCREEN: usize = 8;
const MAX_PAGE_CONNECTIONS: usize = 128;
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const FREE_KEY_FAILURES: u32 = 5;
const FIRST_KEY_BLOCK: Duration = Duration::from_secs(10);
const LONGEST_KEY_BLOCK: Duration = Duration::from_secs(300);
const KEY_FAILURES_FORGOTTEN_AFTER: Duration = Duration::from_secs(600);
const SECURITY_HEADERS: [(header::HeaderName, &str); 4] = [
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; connect-src 'self' ws://127.0.0.1:* ws://localhost:*; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
    ),
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
];

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

/// Slows down guessing of the viewer key: after a few wrong keys, every key is refused for a growing time.
#[derive(Debug, Default)]
struct Throttle {
    failures: u32,
    last_failure: Option<Instant>,
    blocked_until: Option<Instant>,
}

impl Throttle {
    fn blocked(&self, now: Instant) -> bool {
        self.blocked_until.is_some_and(|until| now < until)
    }

    fn fail(&mut self, now: Instant) {
        if self
            .last_failure
            .is_some_and(|last| now.duration_since(last) >= KEY_FAILURES_FORGOTTEN_AFTER)
        {
            self.failures = 0;
        }
        self.failures += 1;
        self.last_failure = Some(now);
        if self.failures >= FREE_KEY_FAILURES {
            let doublings = (self.failures - FREE_KEY_FAILURES).min(8);
            let block = (FIRST_KEY_BLOCK * 2u32.pow(doublings)).min(LONGEST_KEY_BLOCK);
            self.blocked_until = Some(now + block);
        }
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
    throttle: Arc<Mutex<Throttle>>,
}

impl ViewerState {
    /// Checks the viewer key. A wrong key counts against the guess limit.
    fn check_key(&self, headers: &HeaderMap, query: &KeyQuery) -> Result<(), StatusCode> {
        let now = Instant::now();
        let mut throttle = self
            .throttle
            .lock()
            .expect("the throttle is only held for short updates");
        if throttle.blocked(now) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let given = headers
            .get(KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .or(query.key.as_deref());
        if given.is_some_and(|given| keys_match(given.as_bytes(), self.key.as_bytes())) {
            Ok(())
        } else {
            throttle.fail(now);
            Err(StatusCode::UNAUTHORIZED)
        }
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
    let mut response = if host_ok && origin_ok {
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    };
    for (name, value) in SECURITY_HEADERS {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

fn page(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
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
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(&path)),
            )],
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
    state.check_key(&headers, &query)?;
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
    if let Err(status) = state.check_key(&headers, &query) {
        return status.into_response();
    }
    if !valid_screen(screen) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(upstream) = connect_screen(screen).await else {
        return (StatusCode::NOT_FOUND, "this screen is not open").into_response();
    };
    let Some(viewer) = state.hub.attach(screen) else {
        return (StatusCode::GONE, "this screen is closing").into_response();
    };
    let stop = state.stop.clone();
    upgrade.on_upgrade(move |socket| relay_ws(socket, upstream, viewer, stop))
}

/// Tracks whether a WebSocket peer still answers. Any message from the peer counts as an answer.
#[derive(Debug, Default)]
struct Pings {
    awaiting: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum PingStep {
    Send,
    Close,
}

impl Pings {
    fn heard(&mut self) {
        self.awaiting = false;
    }

    /// Called once per ping interval. A peer that said nothing since the last ping is gone.
    fn tick(&mut self) -> PingStep {
        if std::mem::replace(&mut self.awaiting, true) {
            PingStep::Close
        } else {
            PingStep::Send
        }
    }
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
    let mut pings = Pings::default();
    let mut ping_timer = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            _ = ping_timer.tick() => {
                if pings.tick() == PingStep::Close
                    || socket.send(Message::Ping(Vec::new().into())).await.is_err()
                {
                    break;
                }
            }
            message = socket.recv() => {
                pings.heard();
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        if to_vnc.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
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

/// Whether the screen's `Xvnc` accepted the viewer's password.
#[derive(Debug, PartialEq, Eq)]
enum Auth {
    Accepted,
    Refused,
}

async fn forward<R, W>(from: &mut R, to: &mut W, bytes: &mut [u8]) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    from.read_exact(bytes).await?;
    to.write_all(bytes).await
}

/// Relays the RFB handshake between a viewer and `Xvnc` while reading it, and says whether the viewer authenticated.
///
/// Handles protocol versions 3.3 to 3.8 with VNC password authentication, the only method `Xvnc` offers.
async fn relay_handshake<C, U>(client: &mut C, upstream: &mut U) -> io::Result<Auth>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    const VNC_AUTH: u32 = 2;
    let mut version = [0u8; 12];
    forward(upstream, client, &mut version).await?;
    forward(client, upstream, &mut version).await?;
    let minor: u32 = std::str::from_utf8(&version[8..11])
        .ok()
        .and_then(|minor| minor.parse().ok())
        .unwrap_or(8);
    let method = if minor < 7 {
        let mut method = [0u8; 4];
        forward(upstream, client, &mut method).await?;
        u32::from_be_bytes(method)
    } else {
        let mut count = [0u8; 1];
        forward(upstream, client, &mut count).await?;
        if count[0] == 0 {
            return Ok(Auth::Refused);
        }
        let mut offered = vec![0u8; usize::from(count[0])];
        forward(upstream, client, &mut offered).await?;
        let mut chosen = [0u8; 1];
        forward(client, upstream, &mut chosen).await?;
        u32::from(chosen[0])
    };
    if method != VNC_AUTH {
        return Ok(Auth::Refused);
    }
    let mut block = [0u8; 16];
    forward(upstream, client, &mut block).await?;
    forward(client, upstream, &mut block).await?;
    let mut result = [0u8; 4];
    forward(upstream, client, &mut result).await?;
    Ok(if u32::from_be_bytes(result) == 0 {
        Auth::Accepted
    } else {
        Auth::Refused
    })
}

/// Forwards one native VNC client to the screen's `Xvnc`. The client counts as a viewer once it authenticated.
async fn relay_tcp(
    mut client: TcpStream,
    screen: u8,
    hub: Hub,
    stop: CancellationToken,
    _permit: OwnedSemaphorePermit,
) {
    let Ok(mut upstream) = connect_screen(screen).await else {
        debug!(screen, "refused a VNC client, the screen is not open");
        return;
    };
    let handshake = tokio::select! {
        () = stop.cancelled() => return,
        done = tokio::time::timeout(HANDSHAKE_TIMEOUT, relay_handshake(&mut client, &mut upstream)) => done,
    };
    match handshake {
        Ok(Ok(Auth::Accepted)) => {}
        Ok(Ok(Auth::Refused)) => {
            let flush = tokio::io::copy(&mut upstream, &mut client);
            let _ = tokio::time::timeout(REFUSAL_FLUSH_TIMEOUT, flush).await;
            return;
        }
        Ok(Err(_)) | Err(_) => return,
    }
    let Some(_viewer) = hub.attach(screen) else {
        debug!(screen, "refused a VNC client, the screen is closing");
        return;
    };
    tokio::select! {
        () = stop.cancelled() => {}
        _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
    }
}

/// Makes the kernel notice a peer that vanished without closing the connection.
fn keep_alive(stream: &TcpStream) {
    let settings = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    if let Err(error) = socket2::SockRef::from(stream).set_tcp_keepalive(&settings) {
        debug!(%error, "could not turn on TCP keepalive");
    }
}

async fn accept_vnc(listener: TcpListener, screen: u8, hub: Hub, stop: CancellationToken) {
    let connections = TaskTracker::new();
    let room = Arc::new(Semaphore::new(MAX_VNC_CONNECTIONS_PER_SCREEN));
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((client, _)) => {
                    let Ok(permit) = room.clone().try_acquire_owned() else {
                        debug!(screen, "refused a VNC client, the screen has too many connections");
                        continue;
                    };
                    keep_alive(&client);
                    drop(connections.spawn(relay_tcp(client, screen, hub.clone(), stop.clone(), permit)));
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

/// Page server listener that caps open connections and turns on TCP keepalive.
struct Capped {
    inner: TcpListener,
    room: Arc<Semaphore>,
}

/// An accepted page connection that frees its slot when dropped.
struct Held {
    io: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for Held {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Held {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl axum::serve::Listener for Capped {
    type Io = Held;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = Arc::clone(&self.room)
                .acquire_owned()
                .await
                .expect("the connection semaphore is never closed");
            match self.inner.accept().await {
                Ok((io, addr)) => {
                    keep_alive(&io);
                    return (
                        Held {
                            io,
                            _permit: permit,
                        },
                        addr,
                    );
                }
                Err(error) => {
                    warn!(%error, "accepting a page connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
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
        throttle: Arc::default(),
    };
    let page = Capped {
        inner: page,
        room: Arc::new(Semaphore::new(MAX_PAGE_CONNECTIONS)),
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
            throttle: Arc::default(),
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

    /// Plays `Xvnc` for one VNC password authentication. It accepts the response `challenge + 1` per byte.
    async fn fake_xvnc(mut stream: tokio::io::DuplexStream, minor: u8) {
        const CHALLENGE: [u8; 16] = [7; 16];
        stream
            .write_all(
                b"RFB 003.008
",
            )
            .await
            .unwrap();
        let mut version = [0u8; 12];
        stream.read_exact(&mut version).await.unwrap();
        assert_eq!(
            version,
            *format!(
                "RFB 003.00{minor}
"
            )
            .as_bytes()
        );
        if minor < 7 {
            stream.write_all(&2u32.to_be_bytes()).await.unwrap();
        } else {
            stream.write_all(&[1, 2]).await.unwrap();
            let mut chosen = [0u8; 1];
            stream.read_exact(&mut chosen).await.unwrap();
            assert_eq!(chosen, [2]);
        }
        stream.write_all(&CHALLENGE).await.unwrap();
        let mut response = [0u8; 16];
        stream.read_exact(&mut response).await.unwrap();
        let ok = response == CHALLENGE.map(|byte| byte + 1);
        stream
            .write_all(&u32::from(!ok).to_be_bytes())
            .await
            .unwrap();
    }

    async fn handshake_as_client(minor: u8, password_ok: bool) -> Auth {
        let (mut viewer, mut proxy_client) = tokio::io::duplex(1024);
        let (mut proxy_upstream, xvnc) = tokio::io::duplex(1024);
        let viewer_side = async {
            let mut version = [0u8; 12];
            viewer.read_exact(&mut version).await.unwrap();
            viewer
                .write_all(
                    format!(
                        "RFB 003.00{minor}
"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            if minor < 7 {
                let mut method = [0u8; 4];
                viewer.read_exact(&mut method).await.unwrap();
                assert_eq!(u32::from_be_bytes(method), 2);
            } else {
                let mut offered = [0u8; 2];
                viewer.read_exact(&mut offered).await.unwrap();
                assert_eq!(offered, [1, 2]);
                viewer.write_all(&[2]).await.unwrap();
            }
            let mut challenge = [0u8; 16];
            viewer.read_exact(&mut challenge).await.unwrap();
            let response = challenge.map(|byte| if password_ok { byte + 1 } else { byte });
            viewer.write_all(&response).await.unwrap();
            let mut result = [0u8; 4];
            viewer.read_exact(&mut result).await.unwrap();
            assert_eq!(u32::from_be_bytes(result) == 0, password_ok);
        };
        let (auth, (), ()) = tokio::join!(
            async {
                relay_handshake(&mut proxy_client, &mut proxy_upstream)
                    .await
                    .unwrap()
            },
            viewer_side,
            fake_xvnc(xvnc, minor)
        );
        auth
    }

    #[tokio::test]
    async fn the_handshake_relay_reports_whether_the_password_was_accepted() {
        for minor in [3, 7, 8] {
            assert_eq!(
                handshake_as_client(minor, true).await,
                Auth::Accepted,
                "3.{minor}"
            );
            assert_eq!(
                handshake_as_client(minor, false).await,
                Auth::Refused,
                "3.{minor}"
            );
        }
    }

    #[test]
    fn a_silent_websocket_peer_is_closed_after_one_unanswered_ping() {
        let mut pings = Pings::default();
        assert_eq!(pings.tick(), PingStep::Send);
        pings.heard();
        assert_eq!(pings.tick(), PingStep::Send);
        assert_eq!(pings.tick(), PingStep::Close);
    }

    #[test]
    fn repeated_wrong_keys_block_all_keys_for_a_growing_time() {
        let start = Instant::now();
        let mut throttle = Throttle::default();
        for _ in 0..4 {
            throttle.fail(start);
            assert!(!throttle.blocked(start));
        }
        throttle.fail(start);
        assert!(throttle.blocked(start + Duration::from_secs(9)));
        assert!(!throttle.blocked(start + Duration::from_secs(10)));
        let later = start + Duration::from_secs(10);
        throttle.fail(later);
        assert!(throttle.blocked(later + Duration::from_secs(19)));
        assert!(!throttle.blocked(later + Duration::from_secs(20)));
        let much_later = later + KEY_FAILURES_FORGOTTEN_AFTER;
        throttle.fail(much_later);
        assert!(!throttle.blocked(much_later));
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        let root = std::env::temp_dir();
        let (app, _) = app(&root);
        for (uri, host) in [("/", "127.0.0.1:20900"), ("/", "evil.example:20900")] {
            let response = app.clone().oneshot(get(uri, host, None)).await.unwrap();
            let headers = response.headers();
            let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
            assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
            assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
    }

    #[test]
    fn the_key_must_match_in_full() {
        assert!(keys_match(b"abcd2345", b"abcd2345"));
        assert!(!keys_match(b"abcd2346", b"abcd2345"));
        assert!(!keys_match(b"abcd234", b"abcd2345"));
        assert!(!keys_match(b"", b"abcd2345"));
    }
}
