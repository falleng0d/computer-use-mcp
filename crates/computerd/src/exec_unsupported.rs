//! Shell commands run only on Linux.

use std::{path::PathBuf, time::Duration};

use computer_protocol::ShellReply;

#[expect(dead_code, reason = "only the Linux version runs commands")]
pub struct Job {
    pub command: String,
    pub cwd: PathBuf,
    pub display: Option<u8>,
    pub timeout: Duration,
}

#[expect(clippy::unused_async, reason = "same signature as the Linux version")]
pub async fn run(_job: Job) -> anyhow::Result<ShellReply> {
    anyhow::bail!("shell commands need Linux")
}
