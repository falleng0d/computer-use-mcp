//! The `file_transfer` tool: copies files and folders between the host and the computer.

use std::{
    fmt::Write as _,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
    time::Instant,
};

use anyhow::{Context, anyhow};
use computer_protocol::{SessionId, TransferReply, UploadCheck, UploadQuery};
use computer_transfer::{
    Platform, pack,
    pipe::{CHANNEL_CHUNKS, CHUNK, SyncReader, SyncWriter},
    root_name, unpack,
};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::Client;

/// Which way a transfer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    /// Copy `host_path` on the host to `computer_path` on the computer.
    ToComputer,
    /// Copy `computer_path` on the computer to `host_path` on the host.
    FromComputer,
}

/// Where `input` points on the host: `~` is the user's home folder, a relative path starts at `cwd`.
pub(crate) fn host_path(input: &str, home: Option<&Path>, cwd: &Path) -> Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("host_path must not be empty".to_owned());
    }
    let tilde_rest = if input == "~" {
        Some("")
    } else {
        input
            .strip_prefix("~/")
            .or_else(|| input.strip_prefix("~\\"))
    };
    let path = match tilde_rest {
        Some(rest) => {
            let home = home.ok_or("cannot tell where the home folder of this user is")?;
            if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        }
        None => PathBuf::from(input),
    };
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// What the agent reads after a transfer.
pub(crate) fn describe(reply: &TransferReply, took: Duration) -> String {
    let mut text = format!(
        "copied to {}\nfiles: {}\nfolders: {}\nbytes: {}\ntook: {:.1} s",
        reply.path,
        reply.files,
        reply.folders,
        reply.bytes,
        took.as_secs_f64()
    );
    if !reply.skipped.is_empty() {
        let total = reply.skipped.entries.len() as u64 + reply.skipped.omitted;
        let _ = write!(text, "\nskipped {total} entries:");
        for skipped in &reply.skipped.entries {
            let _ = write!(text, "\n{}: {}", skipped.path, skipped.reason);
        }
        if reply.skipped.omitted > 0 {
            let _ = write!(text, "\n... and {} more", reply.skipped.omitted);
        }
    }
    text
}

/// Waits until nothing was taken from the outgoing data for `stall`.
async fn idle_for(last_taken: &Mutex<Instant>, stall: Duration) {
    loop {
        let last = *last_taken
            .lock()
            .expect("the progress time is only set or read");
        let due = last + stall;
        if due <= Instant::now() {
            return;
        }
        tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
    }
}

/// Copies from the host to the computer.
pub(crate) async fn to_computer(
    client: &Client,
    session: &SessionId,
    source: PathBuf,
    computer_path: String,
    overwrite: bool,
) -> anyhow::Result<TransferReply> {
    let meta = tokio::fs::metadata(&source)
        .await
        .with_context(|| format!("cannot read {} on the host", source.display()))?;
    if !meta.is_dir() && !meta.is_file() {
        return Err(anyhow!(
            "{} is a pipe, device, or socket, not a file or folder",
            source.display()
        ));
    }
    let stall = client.stall();
    let name = root_name(&source).with_context(|| format!("cannot send {}", source.display()))?;
    client
        .upload_check(
            session,
            &UploadCheck {
                path: computer_path.clone(),
                overwrite,
                name,
                folder: meta.is_dir(),
            },
        )
        .await?;
    let cancel = CancellationToken::new();
    let _stop_on_drop = cancel.clone().drop_guard();
    let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
    let last_taken = Arc::new(Mutex::new(Instant::now()));
    let writer = SyncWriter::new(tx, cancel.clone(), stall);
    let packing = tokio::task::spawn_blocking(move || {
        let mut out = BufWriter::with_capacity(CHUNK, writer);
        let packed = pack(&source, &mut out, Platform::current());
        if let Err(failure) = &packed {
            out.get_ref()
                .fail(io::Error::other(failure.message.clone()));
        }
        packed
    });
    let progress = last_taken.clone();
    let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, move |mut rx| {
        let progress = progress.clone();
        async move {
            let chunk = rx.recv().await;
            *progress
                .lock()
                .expect("the progress time is only set or read") = Instant::now();
            chunk.map(|chunk| (chunk, rx))
        }
    }));
    let query = UploadQuery {
        path: computer_path,
        overwrite,
    };
    let upload = client.upload(session, &query, body);
    tokio::pin!(upload);
    let sent = tokio::select! {
        sent = &mut upload => sent,
        () = idle_for(&last_taken, stall) => Err(anyhow!(
            "the computer took no data and gave no answer for {} s",
            stall.as_secs_f64()
        )),
    };
    let failed_alone = packing.is_finished();
    cancel.cancel();
    let packed = packing.await.context("reading the files to send")?;
    match (sent, packed) {
        (Ok(reply), _) => Ok(reply),
        (Err(error), Err(failure)) if failed_alone && error.is::<reqwest::Error>() => {
            Err(failure.into())
        }
        (Err(error), _) => Err(error),
    }
}

/// Copies from the computer to the host.
pub(crate) async fn from_computer(
    client: &Client,
    session: &SessionId,
    computer_path: String,
    dest: PathBuf,
    overwrite: bool,
) -> anyhow::Result<TransferReply> {
    let response = client.download(session, computer_path).await?;
    let cancel = CancellationToken::new();
    let _stop_on_drop = cancel.clone().drop_guard();
    let stream = Box::pin(
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(io::Error::other)),
    );
    let reader = SyncReader::new(stream, cancel, client.stall());
    let done =
        tokio::task::spawn_blocking(move || unpack(reader, &dest, overwrite, Platform::current()))
            .await
            .context("writing the files on the host")??;
    Ok(TransferReply {
        path: done.path.display().to_string(),
        files: done.progress.files,
        folders: done.progress.folders,
        bytes: done.progress.bytes,
        skipped: done.skipped,
    })
}

#[cfg(test)]
mod tests {
    use computer_protocol::SkipList;

    use super::*;

    #[test]
    fn host_paths_start_at_home_or_the_working_folder() {
        let home = Path::new("/users/me");
        let cwd = Path::new("/work");
        let at = |input| host_path(input, Some(home), cwd).unwrap();
        assert_eq!(at("~"), home);
        assert_eq!(at("~/Downloads"), home.join("Downloads"));
        assert_eq!(at("~\\Downloads"), home.join("Downloads"));
        assert_eq!(at("out/file.txt"), cwd.join("out/file.txt"));
        assert_eq!(at("~other"), cwd.join("~other"));
        assert!(host_path("  ", Some(home), cwd).is_err());
        assert!(host_path("~/x", None, cwd).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn absolute_windows_paths_are_kept() {
        let got = host_path(r"C:\data\x", None, Path::new(r"D:\work")).unwrap();
        assert_eq!(got, Path::new(r"C:\data\x"));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_paths_are_kept() {
        let got = host_path("/data/x", None, Path::new("/work")).unwrap();
        assert_eq!(got, Path::new("/data/x"));
    }

    #[test]
    fn the_reply_lists_skips_and_counts_the_rest() {
        let mut skipped = SkipList::default();
        for n in 0..52 {
            skipped.push(format!("f{n}"), "a pipe");
        }
        let reply = TransferReply {
            path: "/home/computer/x".to_owned(),
            files: 3,
            folders: 2,
            bytes: 99,
            skipped,
        };
        let text = describe(&reply, Duration::from_millis(1500));
        assert!(
            text.starts_with(
                "copied to /home/computer/x\nfiles: 3\nfolders: 2\nbytes: 99\ntook: 1.5 s"
            ),
            "{text}"
        );
        assert!(text.contains("skipped 52 entries:"), "{text}");
        assert!(
            text.contains("f49: a pipe") && !text.contains("f50:"),
            "{text}"
        );
        assert!(text.ends_with("... and 2 more"), "{text}");
    }

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::computer::Endpoint;

    const NO_CONTENT: &[u8] = b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n";

    /// A stand-in for `computerd` that reads an upload slowly and answers, or never answers.
    async fn slow_computer(read_pause: Duration, slow_bytes: usize, answers: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut chunk = vec![0u8; 32 * 1024];
                    let mut is_check = None;
                    let mut total = 0;
                    loop {
                        let n = conn.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        seen.extend_from_slice(&chunk[..n]);
                        total += n;
                        if is_check.is_none() && seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            is_check =
                                Some(String::from_utf8_lossy(&seen).contains("upload-check"));
                        }
                        match is_check {
                            Some(true) => {
                                let _ = conn.write_all(NO_CONTENT).await;
                                return;
                            }
                            Some(false) if seen.ends_with(b"0\r\n\r\n") => break,
                            _ => {}
                        }
                        let keep = seen.len().saturating_sub(8);
                        seen.drain(..keep);
                        if total < slow_bytes {
                            tokio::time::sleep(read_pause).await;
                        }
                    }
                    if answers {
                        let reply = TransferReply {
                            path: "/home/computer/x".to_owned(),
                            files: 1,
                            folders: 0,
                            bytes: 1,
                            skipped: computer_protocol::SkipList::default(),
                        };
                        let body = serde_json::to_string(&reply).unwrap();
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = conn.write_all(head.as_bytes()).await;
                        let _ = conn.write_all(body.as_bytes()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                });
            }
        });
        port
    }

    /// Answers the destination check, then holds every upload connection open without reading it.
    async fn mute_computer() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut head = vec![0u8; 8192];
                    let n = conn.read(&mut head).await.unwrap_or(0);
                    if String::from_utf8_lossy(&head[..n]).contains("upload-check") {
                        let _ = conn.write_all(NO_CONTENT).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                });
            }
        });
        port
    }

    fn client_for(port: u16, stall: Duration) -> Client {
        let endpoint = Endpoint {
            port,
            token: "t".to_owned(),
            viewer_port: None,
        };
        Client::new(&endpoint).unwrap().with_stall(stall)
    }

    struct Remove(PathBuf);

    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn big_file() -> Remove {
        file_of(16 * 1024 * 1024)
    }

    fn file_of(size: usize) -> Remove {
        let path = std::env::temp_dir().join(format!("computer-use-slow-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, vec![b'x'; size]).unwrap();
        Remove(path)
    }

    fn session() -> SessionId {
        SessionId::parse(&"0".repeat(32)).unwrap()
    }

    #[tokio::test]
    async fn an_upload_that_takes_longer_than_the_stall_succeeds_while_data_keeps_moving() {
        let stall = Duration::from_millis(1500);
        let port = slow_computer(Duration::from_millis(10), 8 * 1024 * 1024, true).await;
        let file = big_file();
        let started = std::time::Instant::now();
        let client = client_for(port, stall);
        let reply = to_computer(&client, &session(), file.0.clone(), "~/x".to_owned(), false)
            .await
            .unwrap();
        assert_eq!(reply.path, "/home/computer/x");
        assert!(started.elapsed() > stall, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn an_upload_fails_when_the_computer_does_not_answer_after_the_data_is_sent() {
        let port = slow_computer(Duration::ZERO, 0, false).await;
        let file = big_file();
        let client = client_for(port, Duration::from_millis(200));
        let error = to_computer(&client, &session(), file.0.clone(), "~/x".to_owned(), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("gave no answer"), "{error}");
    }

    #[tokio::test]
    async fn an_upload_fails_when_the_computer_holds_the_connection_but_stops_reading() {
        let port = mute_computer().await;
        let file = file_of(100 * 1024);
        let client = client_for(port, Duration::from_millis(300));
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(20),
            to_computer(&client, &session(), file.0.clone(), "~/x".to_owned(), false),
        )
        .await
        .expect("the call must not hang")
        .unwrap_err();
        assert!(error.to_string().contains("gave no answer"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(15));
    }
}
