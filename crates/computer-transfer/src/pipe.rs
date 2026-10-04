//! Blocking `Read` and `Write` ends of async streams.
//!
//! The tar code is synchronous and runs in `spawn_blocking`. These types move its bytes through
//! async streams one chunk at a time, so memory stays bounded. Every wait ends when the transfer
//! is cancelled or when no bytes moved for the stall time.

use std::{
    io::{self, Read, Write},
    time::Duration,
};

use futures_util::{Stream, StreamExt};
use tokio::{runtime::Handle, sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

/// Longest a transfer waits for bytes to move before it fails.
pub const STALL: Duration = Duration::from_secs(60);

/// Size of the chunks a [`SyncWriter`] sends.
pub const CHUNK: usize = 256 * 1024;

/// Chunks a transfer channel holds before the writer waits.
pub const CHANNEL_CHUNKS: usize = 4;

/// `io::copy` and `write_all` retry on `ErrorKind::Interrupted`, so a cancelled transfer must not use it.
fn cancelled() -> io::Error {
    io::Error::other("the transfer was cancelled")
}

fn stalled(stall: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no data moved for {} s", stall.as_secs()),
    )
}

/// Reads the chunks of an async stream.
pub struct SyncReader<S, B> {
    handle: Handle,
    stream: S,
    chunk: Option<B>,
    pos: usize,
    cancel: CancellationToken,
    stall: Duration,
}

impl<S, B> SyncReader<S, B>
where
    S: Stream<Item = io::Result<B>> + Unpin,
    B: AsRef<[u8]>,
{
    /// Call from async code, then move the reader into `spawn_blocking`.
    #[must_use]
    pub fn new(stream: S, cancel: CancellationToken, stall: Duration) -> Self {
        Self {
            handle: Handle::current(),
            stream,
            chunk: None,
            pos: 0,
            cancel,
            stall,
        }
    }
}

impl<S, B> Read for SyncReader<S, B>
where
    S: Stream<Item = io::Result<B>> + Unpin,
    B: AsRef<[u8]>,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(chunk) = &self.chunk {
                let rest = &chunk.as_ref()[self.pos..];
                if !rest.is_empty() {
                    let n = rest.len().min(buf.len());
                    buf[..n].copy_from_slice(&rest[..n]);
                    self.pos += n;
                    return Ok(n);
                }
            }
            let next = self.handle.block_on(async {
                tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => Err(cancelled()),
                    next = timeout(self.stall, self.stream.next()) => {
                        next.map_err(|_| stalled(self.stall))
                    }
                }
            })?;
            match next {
                Some(chunk) => {
                    self.chunk = Some(chunk?);
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
    }
}

/// Sends what is written to a bounded channel, in chunks.
pub struct SyncWriter {
    handle: Handle,
    tx: mpsc::Sender<io::Result<Vec<u8>>>,
    cancel: CancellationToken,
    stall: Duration,
}

impl SyncWriter {
    /// Call from async code, then move the writer into `spawn_blocking`.
    #[must_use]
    pub fn new(
        tx: mpsc::Sender<io::Result<Vec<u8>>>,
        cancel: CancellationToken,
        stall: Duration,
    ) -> Self {
        Self {
            handle: Handle::current(),
            tx,
            cancel,
            stall,
        }
    }

    /// Sends a failure after the last chunk so the receiver's stream ends with an error, not early.
    pub fn fail(&self, error: io::Error) {
        self.handle.block_on(async {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => {}
                _ = timeout(self.stall, self.tx.send(Err(error))) => {}
            }
        });
    }
}

impl Write for SyncWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.handle.block_on(async {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => Err(cancelled()),
                sent = timeout(self.stall, self.tx.send(Ok(buf.to_vec()))) => match sent {
                    Err(_) => Err(stalled(self.stall)),
                    Ok(Err(_)) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the other side stopped reading",
                    )),
                    Ok(Ok(())) => Ok(()),
                },
            }
        })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream;

    use super::*;

    #[tokio::test]
    async fn reader_hands_over_chunks_in_order_across_small_reads() {
        let chunks = vec![
            Ok(b"hello ".to_vec()),
            Ok(b"wor".to_vec()),
            Ok(b"ld".to_vec()),
        ];
        let reader = SyncReader::new(
            Box::pin(stream::iter(chunks)),
            CancellationToken::new(),
            STALL,
        );
        let text = tokio::task::spawn_blocking(move || {
            let mut text = String::new();
            let mut reader = io::BufReader::with_capacity(4, reader);
            reader.read_to_string(&mut text).unwrap();
            text
        })
        .await
        .unwrap();
        assert_eq!(text, "hello world");
    }

    #[tokio::test]
    async fn reader_fails_when_no_bytes_arrive_for_the_stall_time() {
        let pending = stream::pending::<io::Result<Vec<u8>>>();
        let mut reader = SyncReader::new(
            Box::pin(pending),
            CancellationToken::new(),
            Duration::from_millis(50),
        );
        let error = tokio::task::spawn_blocking(move || reader.read(&mut [0; 8]).unwrap_err())
            .await
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn reader_stops_when_cancelled_while_waiting() {
        let cancel = CancellationToken::new();
        let mut reader = SyncReader::new(
            Box::pin(stream::pending::<io::Result<Vec<u8>>>()),
            cancel.clone(),
            STALL,
        );
        let read = tokio::task::spawn_blocking(move || reader.read(&mut [0; 8]).unwrap_err());
        cancel.cancel();
        let error = read.await.unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "the transfer was cancelled");
    }

    #[tokio::test]
    async fn io_copy_gives_up_on_a_cancelled_reader_instead_of_retrying() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut reader = SyncReader::new(
            Box::pin(stream::pending::<io::Result<Vec<u8>>>()),
            cancel,
            STALL,
        );
        let copy = tokio::task::spawn_blocking(move || io::copy(&mut reader, &mut io::sink()));
        let finished = timeout(Duration::from_secs(5), copy).await;
        assert!(finished.unwrap().unwrap().is_err());
    }

    #[tokio::test]
    async fn writer_fails_when_nobody_reads_for_the_stall_time() {
        let (tx, _rx) = mpsc::channel(1);
        let mut writer = SyncWriter::new(tx, CancellationToken::new(), Duration::from_millis(50));
        let error = tokio::task::spawn_blocking(move || {
            writer.write_all(b"one").unwrap();
            writer.write_all(b"two").unwrap_err()
        })
        .await
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn writer_stops_when_the_receiver_is_gone() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut writer = SyncWriter::new(tx, CancellationToken::new(), STALL);
        let error = tokio::task::spawn_blocking(move || writer.write_all(b"x").unwrap_err())
            .await
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
