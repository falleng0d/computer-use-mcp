//! Runs shell commands for sessions.

use std::{
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::Context;
use computer_protocol::{ShellOutcome, ShellReply};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::{
    cap::Capture,
    env::{self, SHELL},
    proc,
};

/// Time the pipes get to reach end of file after the shell exits. A background job that
/// kept them open is not waited for.
const DRAIN_GRACE: Duration = Duration::from_millis(300);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_CHUNK: usize = 8192;

/// One command to run.
pub struct Job {
    pub command: String,
    pub cwd: PathBuf,
    pub display: Option<u8>,
    pub timeout: Duration,
    /// Kills the command when cancelled.
    pub cancel: CancellationToken,
}

/// Kills the process group when dropped while armed, so a dropped call leaves nothing running.
struct GroupKill(Option<u32>);

impl GroupKill {
    fn kill(&mut self) {
        if let Some(group) = self.0.take() {
            proc::kill_group(group);
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn pump(mut stream: impl AsyncRead + Unpin, capture: &mut Capture) {
    let mut buffer = vec![0; READ_CHUNK];
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        capture.push(&buffer[..read]);
    }
}

/// Runs `bash -lc` in its own process group and waits for it, killing the group on timeout.
///
/// A command that is still running when the caller drops this future is killed as well.
///
/// # Errors
///
/// Fails when the shell cannot be started. A command that fails or times out is a normal reply.
pub async fn run(job: Job) -> anyhow::Result<ShellReply> {
    let started = Instant::now();
    let mut command = Command::new(SHELL);
    command
        .args(["-lc", &job.command])
        .current_dir(&job.cwd)
        .env_clear()
        .envs(env::from_process(job.display))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| {
        if job.cwd.is_dir() {
            format!("starting bash in {}", job.cwd.display())
        } else {
            format!(
                "the working folder {} no longer exists, call set_cwd to choose another",
                job.cwd.display()
            )
        }
    })?;
    let mut group = GroupKill(child.id());
    let stdout = child.stdout.take().context("bash has no stdout pipe")?;
    let stderr = child.stderr.take().context("bash has no stderr pipe")?;
    let (mut out, mut err) = (Capture::default(), Capture::default());

    let finished = tokio::time::timeout(job.timeout, async {
        let mut read = std::pin::pin!(async {
            tokio::join!(pump(stdout, &mut out), pump(stderr, &mut err));
        });
        tokio::select! {
            status = child.wait() => {
                group.disarm();
                let _ = tokio::time::timeout(DRAIN_GRACE, &mut read).await;
                status
            }
            () = &mut read => {
                let status = child.wait().await;
                group.disarm();
                status
            }
        }
    });
    let finished = tokio::select! {
        finished = finished => Some(finished),
        () = job.cancel.cancelled() => None,
    };

    let outcome = if let Some(Ok(status)) = finished {
        let status = status.context("waiting for bash")?;
        match (status.code(), status.signal()) {
            (Some(code), _) => ShellOutcome::Exited { code },
            (None, Some(signal)) => ShellOutcome::Signaled { signal },
            (None, None) => ShellOutcome::Exited { code: -1 },
        }
    } else {
        group.kill();
        let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
        if finished.is_none() {
            ShellOutcome::Cancelled
        } else {
            ShellOutcome::TimedOut {
                after_secs: job.timeout.as_secs(),
            }
        }
    };
    Ok(ShellReply {
        outcome,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        stdout: out.into_text(),
        stderr: err.into_text(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(command: &str, timeout: Duration) -> Job {
        Job {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            display: None,
            timeout,
            cancel: CancellationToken::new(),
        }
    }

    fn pgrep(marker: &str) -> Vec<u8> {
        std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .unwrap()
            .stdout
    }

    #[tokio::test]
    async fn exit_code_and_both_streams_are_reported() {
        let reply = run(job(
            "echo hi; echo err >&2; exit 3",
            Duration::from_secs(10),
        ))
        .await
        .unwrap();
        assert_eq!(reply.outcome, ShellOutcome::Exited { code: 3 });
        assert_eq!(reply.stdout, "hi\n");
        assert_eq!(reply.stderr, "err\n");
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group_and_keeps_output_so_far() {
        let marker = format!("computerd-test-{}", std::process::id());
        let command = format!("echo started; (exec -a {marker} sleep 1000) & wait");
        let reply = run(job(&command, Duration::from_secs(1))).await.unwrap();
        assert_eq!(reply.outcome, ShellOutcome::TimedOut { after_secs: 1 });
        assert_eq!(reply.stdout, "started\n");
        assert!(pgrep(&marker).is_empty(), "a child survived the timeout");
    }

    #[tokio::test]
    async fn dropping_the_call_kills_the_group() {
        let marker = format!("computerd-drop-{}", std::process::id());
        let command = format!("(exec -a {marker} sleep 1000) & wait");
        let call = run(job(&command, Duration::from_secs(60)));
        let _ = tokio::time::timeout(Duration::from_millis(500), call).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            pgrep(&marker).is_empty(),
            "a child survived the dropped call"
        );
    }

    #[tokio::test]
    async fn cancelling_kills_the_group_and_returns_at_once() {
        let marker = format!("computerd-cancel-{}", std::process::id());
        let command = format!("echo begun; (exec -a {marker} sleep 1000) & wait");
        let running = job(&command, Duration::from_secs(60));
        let cancel = running.cancel.clone();
        let started = Instant::now();
        let (reply, ()) = tokio::join!(run(running), async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.cancel();
        });
        let reply = reply.unwrap();
        assert_eq!(reply.outcome, ShellOutcome::Cancelled);
        assert_eq!(
            reply.stdout,
            "begun
"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(pgrep(&marker).is_empty(), "a child survived the cancel");
    }

    #[tokio::test]
    async fn a_background_job_holding_the_pipes_does_not_delay_the_reply() {
        let started = Instant::now();
        let reply = run(job("echo done; sleep 5 &", Duration::from_secs(30)))
            .await
            .unwrap();
        assert_eq!(reply.stdout, "done\n");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
