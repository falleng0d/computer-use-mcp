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

use crate::cap::Capture;

/// Time the pipes get to reach end of file after the shell exits. A background job that
/// kept them open is not waited for.
const DRAIN_GRACE: Duration = Duration::from_millis(300);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_CHUNK: usize = 8192;
const DEFAULT_USER: &str = "computer";
const SHELL: &str = "/bin/bash";

/// One command to run.
pub struct Job {
    pub command: String,
    pub cwd: PathBuf,
    pub display: Option<u8>,
    pub timeout: Duration,
}

/// Environment of a command: a few variables taken from the daemon's own, never its secrets.
pub fn environment(
    parent: impl Fn(&str) -> Option<String>,
    display: Option<u8>,
) -> Vec<(&'static str, String)> {
    let user = parent("USER").unwrap_or_else(|| DEFAULT_USER.to_owned());
    let mut env = vec![
        (
            "HOME",
            parent("HOME").unwrap_or_else(|| format!("/home/{DEFAULT_USER}")),
        ),
        ("LOGNAME", user.clone()),
        ("USER", user),
        ("SHELL", SHELL.to_owned()),
        ("TERM", "dumb".to_owned()),
    ];
    for name in ["PATH", "LANG", "LC_ALL", "TZ"] {
        if let Some(value) = parent(name) {
            env.push((name, value));
        }
    }
    if let Some(number) = display {
        env.push(("DISPLAY", format!(":{number}")));
    }
    env
}

/// Kills the process group when dropped while armed, so a dropped call leaves nothing running.
struct GroupKill(Option<i32>);

impl GroupKill {
    fn kill(&mut self) {
        if let Some(group) = self.0.take() {
            // SAFETY: killpg takes plain integers and has no memory effects. At worst the group is gone.
            unsafe { libc::killpg(group, libc::SIGKILL) };
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
        .envs(environment(|name| std::env::var(name).ok(), job.display))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting bash in {}", job.cwd.display()))?;
    let mut group = GroupKill(child.id().and_then(|pid| i32::try_from(pid).ok()));
    let stdout = child.stdout.take().context("bash has no stdout pipe")?;
    let stderr = child.stderr.take().context("bash has no stderr pipe")?;
    let (mut out, mut err) = (Capture::default(), Capture::default());

    let finished = tokio::time::timeout(job.timeout, async {
        let mut read = std::pin::pin!(async {
            tokio::join!(pump(stdout, &mut out), pump(stderr, &mut err));
        });
        let status = tokio::select! {
            status = child.wait() => {
                let _ = tokio::time::timeout(DRAIN_GRACE, &mut read).await;
                status
            }
            () = &mut read => child.wait().await,
        };
        group.disarm();
        status
    })
    .await;

    let outcome = if let Ok(status) = finished {
        let status = status.context("waiting for bash")?;
        match (status.code(), status.signal()) {
            (Some(code), _) => ShellOutcome::Exited { code },
            (None, Some(signal)) => ShellOutcome::Signaled { signal },
            (None, None) => ShellOutcome::Exited { code: -1 },
        }
    } else {
        group.kill();
        let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
        ShellOutcome::TimedOut {
            after_secs: job.timeout.as_secs(),
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
        }
    }

    fn pgrep(marker: &str) -> Vec<u8> {
        std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .unwrap()
            .stdout
    }

    #[test]
    fn environment_passes_the_login_basics_and_never_the_token() {
        let parent = |name: &str| match name {
            "PATH" => Some("/usr/bin".to_owned()),
            "HOME" => Some("/home/computer".to_owned()),
            "COMPUTERD_TOKEN" => Some("secret".to_owned()),
            _ => None,
        };
        let with_screen = environment(parent, Some(3));
        let get = |env: &[(&str, String)], key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(get(&with_screen, "DISPLAY"), Some(":3".to_owned()));
        assert_eq!(get(&with_screen, "USER"), Some("computer".to_owned()));
        assert_eq!(get(&with_screen, "PATH"), Some("/usr/bin".to_owned()));
        assert_eq!(get(&with_screen, "COMPUTERD_TOKEN"), None);
        assert_eq!(get(&environment(parent, None), "DISPLAY"), None);
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
    async fn a_background_job_holding_the_pipes_does_not_delay_the_reply() {
        let started = Instant::now();
        let reply = run(job("echo done; sleep 5 &", Duration::from_secs(30)))
            .await
            .unwrap();
        assert_eq!(reply.stdout, "done\n");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
