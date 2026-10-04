//! A small client for the Chromium `DevTools` protocol on the container's loopback.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(8);
const PAGE_POLL: Duration = Duration::from_millis(100);
/// Largest HTTP reply read from the endpoint.
const MAX_HTTP_BYTES: u64 = 1 << 20;

/// One `DevTools` WebSocket, used for one request at a time.
pub(crate) struct Connection {
    socket: WebSocketStream<TcpStream>,
    next_id: u64,
}

/// Splits an HTTP reply into the length of its header block and the body length it announces.
fn parse_head(reply: &[u8]) -> Option<(usize, usize)> {
    let end = reply.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&reply[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())?;
    Some((end, length))
}

async fn http_get(port: u16, path: &str) -> Result<Value> {
    let fetch = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await?;
        let mut reply = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            if let Some((head, length)) = parse_head(&reply) {
                if length as u64 > MAX_HTTP_BYTES {
                    bail!("DevTools sent a reply that is too large");
                }
                if reply.len() >= head + length {
                    return Ok(reply.split_off(head));
                }
            }
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                bail!("DevTools closed the connection before it finished the reply");
            }
            reply.extend_from_slice(&chunk[..read]);
        }
    };
    let body = tokio::time::timeout(HTTP_TIMEOUT, fetch)
        .await
        .context("DevTools did not answer in time")?
        .context("asking DevTools over HTTP")?;
    serde_json::from_slice(&body).context("reading the DevTools reply")
}

impl Connection {
    /// Connects to the browser target of the Chromium on `port`.
    pub(crate) async fn browser(port: u16) -> Result<Self> {
        let version = http_get(port, "/json/version").await?;
        let url = version
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .context("DevTools gave no browser WebSocket address")?;
        Self::open(port, url).await
    }

    async fn open(port: u16, url: &str) -> Result<Self> {
        let connect = async {
            let stream = TcpStream::connect(("127.0.0.1", port)).await?;
            let (socket, _) = client_async(url, stream).await?;
            anyhow::Ok(socket)
        };
        let socket = tokio::time::timeout(HTTP_TIMEOUT, connect)
            .await
            .context("DevTools did not accept the WebSocket in time")?
            .context("opening the DevTools WebSocket")?;
        Ok(Self { socket, next_id: 0 })
    }

    /// Calls `method` and returns its result. Events that arrive meanwhile are skipped.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let body = json!({"id": id, "method": method, "params": params}).to_string();
        let exchange = async {
            self.socket.send(Message::text(body)).await?;
            loop {
                let message = self
                    .socket
                    .next()
                    .await
                    .ok_or_else(|| anyhow!("DevTools closed the connection"))??;
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(reply) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if reply.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = reply.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
                    bail!("{method} failed with {code} {message}");
                }
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(CALL_TIMEOUT, exchange)
            .await
            .with_context(|| format!("{method} did not answer in time"))?
    }

    /// Every cookie of the default browser context.
    pub(crate) async fn get_cookies(&mut self) -> Result<Vec<Value>> {
        let result = self.call("Storage.getCookies", json!({})).await?;
        Ok(result
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Writes `cookies`, which are `Storage.setCookies` parameters.
    pub(crate) async fn set_cookies(&mut self, cookies: Vec<Value>) -> Result<()> {
        self.call("Storage.setCookies", json!({"cookies": cookies}))
            .await
            .map(|_| ())
    }
}

/// Loads `url` in the first tab of the Chromium on `port`.
pub(crate) async fn navigate_first_tab(port: u16, url: &str) -> Result<()> {
    let navigate = async {
        let socket_url = loop {
            let tabs = http_get(port, "/json/list").await?;
            let found = tabs
                .as_array()
                .into_iter()
                .flatten()
                .find(|tab| tab.get("type").and_then(Value::as_str) == Some("page"))
                .and_then(|tab| tab.get("webSocketDebuggerUrl")?.as_str().map(str::to_owned));
            if let Some(found) = found {
                break found;
            }
            tokio::time::sleep(PAGE_POLL).await;
        };
        let mut tab = Connection::open(port, &socket_url).await?;
        tab.call("Page.navigate", json!({"url": url})).await?;
        anyhow::Ok(())
    };
    tokio::time::timeout(NAVIGATE_TIMEOUT, navigate)
        .await
        .context("the first tab did not show up in time")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_length_comes_from_the_content_length_header() {
        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\nabcdefghijk";
        assert_eq!(parse_head(reply), Some((reply.len() - 11, 11)));
        assert_eq!(parse_head(b"HTTP/1.1 200 OK\r\nContent-Len"), None);
    }
}
