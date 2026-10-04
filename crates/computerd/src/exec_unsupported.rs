//! Shell commands run only on Linux.

use std::{path::PathBuf, time::Duration};

use computer_protocol::ShellReply;

#[expect(dead_code, reason = "only the Linux version runs commands")]
pub(crate) struct Job {
    pub(crate) command: String,
    pub(crate) cwd: PathBuf,
    pub(crate) display: Option<u8>,
    pub(crate) timeout: Duration,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
}

#[expect(clippy::unused_async, reason = "same signature as the Linux version")]
pub(crate) async fn run(_job: Job) -> anyhow::Result<ShellReply> {
    anyhow::bail!("shell commands need Linux")
}
