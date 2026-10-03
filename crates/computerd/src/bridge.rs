//! A loopback port that programs on a screen use to ask for its browser, such as the Fluxbox menu.
//!
//! The port is not published. A request is one text line, `browser <screen> [url]`, and the
//! answer is `ok` or `error <message>`. Other text, such as a web page's HTTP request, is refused.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use computer_protocol::SCREEN_COUNT;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::sessions::Sessions;

pub const PORT: u16 = 7071;
const MAX_LINE_BYTES: u64 = 4096;
/// Requests handled at once. Others wait in the listen queue.
const MAX_REQUESTS: usize = 16;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest wait for an answer, which covers starting the browser.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);
const COMMAND: &str = "browser";

/// A request for the browser of one screen.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub screen: u8,
    pub url: Option<String>,
}

impl Request {
    /// The request as it goes over the wire, without the line end.
    pub fn line(&self) -> String {
        match &self.url {
            Some(url) => format!("{COMMAND} {} {url}", self.screen),
            None => format!("{COMMAND} {}", self.screen),
        }
    }

    /// Reads one request line.
    ///
    /// # Errors
    ///
    /// Fails when the line is not `browser <screen> [url]` with a screen from 1 to 16.
    pub fn parse(line: &str) -> Result<Self, String> {
        let mut parts = line.trim().splitn(3, ' ');
        let command = parts.next().unwrap_or_default();
        let screen = parts
            .next()
            .and_then(|screen| screen.parse::<u8>().ok())
            .filter(|screen| (1..=SCREEN_COUNT).contains(screen));
        match (command, screen) {
            (COMMAND, Some(screen)) => Ok(Self {
                screen,
                url: parts
                    .next()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .map(str::to_owned),
            }),
            _ => Err(format!(
                "expected `{COMMAND} <screen from 1 to {SCREEN_COUNT}> [url]`"
            )),
        }
    }
}

/// Answers requests until `stop` is cancelled. Requests being handled are dropped then.
pub async fn serve(listener: TcpListener, sessions: Sessions, stop: CancellationToken) {
    let mut handlers = JoinSet::new();
    let slots = Arc::new(Semaphore::new(MAX_REQUESTS));
    loop {
        let acquired = tokio::select! {
            () = stop.cancelled() => None,
            slot = Arc::clone(&slots).acquire_owned() => slot.ok(),
        };
        let Some(slot) = acquired else {
            break;
        };
        tokio::select! {
            () = stop.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let sessions = sessions.clone();
                    handlers.spawn(async move {
                        answer(stream, sessions).await;
                        drop(slot);
                    });
                }
                Err(error) => {
                    warn!(%error, "could not accept a browser request");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = handlers.join_next() => {}
        }
    }
    handlers.shutdown().await;
}

async fn answer(stream: TcpStream, sessions: Sessions) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_LINE_BYTES));
    let reply = match tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(count)) if count > 0 && line.ends_with('\n') => match Request::parse(&line) {
            Ok(request) => sessions
                .show_browser(request.screen, request.url)
                .await
                .map_or_else(|error| format!("error {error}"), |()| "ok".to_owned()),
            Err(message) => format!("error {message}"),
        },
        _ => return,
    };
    let _ = tokio::time::timeout(
        READ_TIMEOUT,
        write.write_all(format!("{reply}\n").as_bytes()),
    )
    .await;
}

/// Asks the running `computerd` to show the browser of `request.screen`.
///
/// # Errors
///
/// Fails when `computerd` cannot be reached or answers with an error.
pub async fn ask(request: &Request) -> Result<()> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(("127.0.0.1", PORT)))
        .await
        .context("connecting to computerd timed out")?
        .context("connecting to computerd")?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{}\n", request.line()).as_bytes())
        .await
        .context("sending the request")?;
    let mut answer = String::new();
    tokio::time::timeout(ANSWER_TIMEOUT, BufReader::new(read).read_line(&mut answer))
        .await
        .context("computerd did not answer in time")?
        .context("reading the answer")?;
    match answer.trim() {
        "ok" => Ok(()),
        other => bail!("{}", other.strip_prefix("error ").unwrap_or(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_name_a_screen_and_may_carry_a_url() {
        assert_eq!(
            Request::parse("browser 3\n"),
            Ok(Request {
                screen: 3,
                url: None
            })
        );
        let with_url = Request {
            screen: 16,
            url: Some("https://a.test/x y".to_owned()),
        };
        assert_eq!(
            Request::parse(&format!("{}\n", with_url.line())),
            Ok(with_url)
        );
        for refused in [
            "browser 0",
            "browser 17",
            "browser x",
            "open 1",
            "POST / HTTP/1.1",
            "",
        ] {
            assert!(Request::parse(refused).is_err(), "{refused:?}");
        }
    }
}
