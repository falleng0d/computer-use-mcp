//! Streams files and folders between the host and this computer as tar archives.

use std::{
    io::{self, BufWriter},
    path::PathBuf,
};

use axum::body::Body;
use computer_protocol::TransferReply;
use computer_transfer::{
    Platform,
    pipe::{CHANNEL_CHUNKS, CHUNK, STALL, SyncReader, SyncWriter},
    unpack,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::warn;

use crate::sessions::SessionError;

/// Writes the archive in `body` to `dest`. Returns what was written.
pub(crate) async fn upload(
    dest: PathBuf,
    overwrite: bool,
    body: Body,
    cancel: CancellationToken,
) -> Result<TransferReply, SessionError> {
    let stream = Box::pin(
        body.into_data_stream()
            .map(|chunk| chunk.map_err(io::Error::other)),
    );
    let reader = SyncReader::new(stream, cancel, STALL);
    let done =
        tokio::task::spawn_blocking(move || unpack(reader, &dest, overwrite, Platform::current()))
            .await
            .map_err(|error| {
                SessionError::Failed(anyhow::Error::new(error).context("unpacking the transfer"))
            })?
            .map_err(|failure| SessionError::Rejected(failure.to_string()))?;
    Ok(TransferReply {
        path: done.path.display().to_string(),
        files: done.progress.files,
        folders: done.progress.folders,
        bytes: done.progress.bytes,
        skipped: done.skipped,
    })
}

/// Starts packing `source` and returns the archive as a response body.
///
/// `keep` stays alive until packing ends, so the call counts as running. Packing stops when the
/// body is dropped, when `cancel` fires, or when the receiver stops reading for too long.
pub(crate) fn download<K: Send + 'static>(
    source: PathBuf,
    cancel: CancellationToken,
    tasks: &TaskTracker,
    keep: K,
) -> Body {
    let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
    let writer = SyncWriter::new(tx, cancel, STALL);
    drop(tasks.spawn_blocking(move || {
        let _keep = keep;
        let mut out = BufWriter::with_capacity(CHUNK, writer);
        if let Err(failure) = computer_transfer::pack(&source, &mut out, Platform::current()) {
            warn!(error = %failure, "sending a path to the host failed");
            out.get_ref().fail(io::Error::other(failure.message));
        }
    }));
    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    }))
}
