//! The `file_transfer` tool: copies files and folders between the host and the computer.

use std::{
    fmt::Write as _,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, anyhow};
use computer_protocol::{SessionId, TransferReply, UploadQuery};
use computer_transfer::{
    Platform, pack,
    pipe::{CHANNEL_CHUNKS, CHUNK, STALL, SyncReader, SyncWriter},
    unpack,
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
    let cancel = CancellationToken::new();
    let _stop_on_drop = cancel.clone().drop_guard();
    let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
    let writer = SyncWriter::new(tx, cancel.clone(), STALL);
    let packing = tokio::task::spawn_blocking(move || {
        let mut out = BufWriter::with_capacity(CHUNK, writer);
        let packed = pack(&source, &mut out, Platform::current());
        if let Err(failure) = &packed {
            out.get_ref()
                .fail(io::Error::other(failure.message.clone()));
        }
        packed
    });
    let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    }));
    let query = UploadQuery {
        path: computer_path,
        overwrite,
    };
    let sent = client.upload(session, &query, body).await;
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
    let reader = SyncReader::new(stream, cancel, STALL);
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
}
