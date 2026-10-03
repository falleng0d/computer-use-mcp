use std::{
    collections::BTreeSet,
    future::Future,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use computer_protocol::{Observation, ScreenSize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::{
    frames::{self, FrameTracker},
    x11::Capturer,
};

pub const FIRST_SCREEN: u8 = 1;
pub const LAST_SCREEN: u8 = 16;

const FLUXBOX_INIT: &str = "/etc/computerd/fluxbox-init";
const X_SOCKET_DIR: &str = "/tmp/.X11-unix";
const X_READY_TIMEOUT: Duration = Duration::from_secs(10);
const WM_READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Lowest screen number from [`FIRST_SCREEN`] to [`LAST_SCREEN`] that is not in `used`.
pub fn lowest_free(used: &BTreeSet<u8>) -> Option<u8> {
    (FIRST_SCREEN..=LAST_SCREEN).find(|number| !used.contains(number))
}

/// The screen numbers in use. A [`Lease`] frees its number when dropped.
#[derive(Debug, Clone, Default)]
pub struct Numbers(Arc<Mutex<BTreeSet<u8>>>);

#[derive(Debug)]
pub struct Lease {
    numbers: Numbers,
    number: u8,
}

impl Numbers {
    /// Takes the lowest free number, or `None` when all 16 screens exist.
    pub fn lease(&self) -> Option<Lease> {
        let mut used = self
            .0
            .lock()
            .expect("the screen number lock is only held for short set updates");
        let number = lowest_free(&used)?;
        used.insert(number);
        Some(Lease {
            numbers: self.clone(),
            number,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.numbers
            .0
            .lock()
            .expect("the screen number lock is only held for short set updates")
            .remove(&self.number);
    }
}

/// Removes lock and socket files that a killed X server left behind.
pub fn clean_stale_x_files() {
    for number in FIRST_SCREEN..=LAST_SCREEN {
        let _ = std::fs::remove_file(lock_path(number));
        let _ = std::fs::remove_file(socket_path(number));
    }
}

fn lock_path(number: u8) -> PathBuf {
    PathBuf::from(format!("/tmp/.X{number}-lock"))
}

fn socket_path(number: u8) -> PathBuf {
    PathBuf::from(X_SOCKET_DIR).join(format!("X{number}"))
}

fn display(number: u8) -> String {
    format!(":{number}")
}

/// The `Xvnc` and Fluxbox child processes of one screen.
struct Processes {
    number: u8,
    xvnc: Child,
    fluxbox: Option<Child>,
}

impl Processes {
    fn start_xvnc(number: u8, size: ScreenSize) -> Result<Self> {
        let xvnc = Command::new("Xvnc")
            .arg(display(number))
            .args(["-geometry", &size.to_string(), "-depth", "24"])
            .args(["-AcceptSetDesktopSize=0", "-nolisten", "tcp"])
            .args(["-localhost", "-SecurityTypes", "None"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting Xvnc")?;
        Ok(Self {
            number,
            xvnc,
            fluxbox: None,
        })
    }

    fn start_fluxbox(&mut self) -> Result<()> {
        let fluxbox = Command::new("fluxbox")
            .args(["-display", &display(self.number), "-rc", FLUXBOX_INIT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting Fluxbox")?;
        self.fluxbox = Some(fluxbox);
        Ok(())
    }

    /// Fails when a child has already exited.
    fn check_alive(&mut self) -> Result<()> {
        if let Some(status) = self.xvnc.try_wait()? {
            bail!("Xvnc exited early with {status}");
        }
        if let Some(child) = self.fluxbox.as_mut()
            && let Some(status) = child.try_wait()?
        {
            bail!("Fluxbox exited early with {status}");
        }
        Ok(())
    }

    /// Kills both children, waits for them, and removes the X files they leave.
    async fn stop(mut self) {
        for (name, child) in [
            ("Fluxbox", self.fluxbox.as_mut()),
            ("Xvnc", Some(&mut self.xvnc)),
        ] {
            let Some(child) = child else { continue };
            if let Err(error) = child.start_kill() {
                warn!(process = name, %error, "could not kill process");
            }
            if tokio::time::timeout(STOP_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                warn!(process = name, "process did not exit after being killed");
            }
        }
        let _ = std::fs::remove_file(lock_path(self.number));
        let _ = std::fs::remove_file(socket_path(self.number));
    }
}

/// One X display with its window manager and capture connection.
pub struct Screen {
    _lease: Lease,
    number: u8,
    size: ScreenSize,
    processes: Processes,
    source: Arc<Mutex<Source>>,
}

/// Everything a capture touches, locked together inside `spawn_blocking`.
struct Source {
    capturer: Capturer,
    frames: FrameTracker,
}

impl Screen {
    /// Starts `Xvnc` and Fluxbox on the leased number and waits until both are usable.
    pub async fn open(lease: Lease, size: ScreenSize) -> Result<Self> {
        let number = lease.number;
        let mut processes = Processes::start_xvnc(number, size)?;
        match Self::bring_up(&mut processes, number, size).await {
            Ok(source) => {
                info!(screen = number, %size, "screen opened");
                Ok(Self {
                    _lease: lease,
                    number,
                    size,
                    processes,
                    source,
                })
            }
            Err(error) => {
                processes.stop().await;
                Err(error.context(format!("opening screen {number}")))
            }
        }
    }

    async fn bring_up(
        processes: &mut Processes,
        number: u8,
        size: ScreenSize,
    ) -> Result<Arc<Mutex<Source>>> {
        let capturer = retry_until(X_READY_TIMEOUT, "Xvnc to accept connections", || {
            processes.check_alive()?;
            Ok(blocking(move || Capturer::connect(&display(number), size)))
        })
        .await?
        .context("connecting to the display")?;
        let source = Arc::new(Mutex::new(Source {
            capturer,
            frames: FrameTracker::default(),
        }));
        processes.start_fluxbox()?;
        let ready = retry_until(WM_READY_TIMEOUT, "Fluxbox to start", || {
            processes.check_alive()?;
            let source = Arc::clone(&source);
            Ok(blocking(move || {
                let source = source
                    .lock()
                    .expect("the capture lock is only held inside spawn_blocking");
                match source.capturer.window_manager_ready() {
                    Ok(true) => Ok(()),
                    Ok(false) => bail!("the window manager has not announced itself"),
                    Err(error) => Err(error),
                }
            }))
        })
        .await?;
        if let Err(error) = ready {
            warn!(screen = number, error = %format!("{error:#}"), "continuing without a ready window manager");
        }
        Ok(source)
    }

    /// Fails when `Xvnc` or Fluxbox has exited.
    pub fn check_alive(&mut self) -> Result<()> {
        self.processes.check_alive()
    }

    pub fn number(&self) -> u8 {
        self.number
    }

    /// Captures the screen. The image is left out when the session already has this frame.
    pub async fn observe(&mut self) -> Result<Observation> {
        let source = Arc::clone(&self.source);
        let size = self.size;
        tokio::task::spawn_blocking(move || capture(&source, size))
            .await
            .context("running the capture")?
    }

    /// Stops the screen's processes. The screen number frees when `self` drops.
    pub async fn close(self) {
        info!(screen = self.number, "closing screen");
        self.processes.stop().await;
    }
}

fn capture(source: &Mutex<Source>, size: ScreenSize) -> Result<Observation> {
    let mut guard = source
        .lock()
        .expect("the capture lock is only held inside spawn_blocking");
    let Source { capturer, frames } = &mut *guard;
    let frame = frames.observe(capturer.take_damage()?);
    let cursor = capturer.cursor()?;
    let active_window = capturer.active_window_title();
    let png_base64 = if frame.send_image {
        let rgb = frames::bgrx_to_rgb(capturer.grab()?);
        let png = frames::encode_png(size.width(), size.height(), &rgb)?;
        frames.delivered();
        Some(STANDARD.encode(png))
    } else {
        None
    };
    Ok(Observation {
        frame_id: frame.id,
        captured_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("formatting the capture time")?,
        width: size.width(),
        height: size.height(),
        cursor,
        active_window,
        png_base64,
    })
}

/// Runs blocking X11 work on the blocking pool with a time limit, so a silent
/// server cannot hold up an async worker.
fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> impl Future<Output = Result<T>> {
    let task = tokio::task::spawn_blocking(work);
    async move {
        match tokio::time::timeout(ATTEMPT_TIMEOUT, task).await {
            Ok(joined) => joined.context("running blocking work")?,
            Err(_) => bail!("no answer within {} s", ATTEMPT_TIMEOUT.as_secs()),
        }
    }
}

/// Repeats `attempt` until it succeeds or `limit` passes.
///
/// The outer error is fatal and ends the wait at once. The inner error reports
/// a timeout and carries the last failed attempt's error.
async fn retry_until<T, Fut>(
    limit: Duration,
    what: &str,
    mut attempt: impl FnMut() -> Result<Fut>,
) -> Result<Result<T>>
where
    Fut: Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let last_error = match attempt()?.await {
            Ok(value) => return Ok(Ok(value)),
            Err(error) => error,
        };
        if tokio::time::Instant::now() >= deadline {
            return Ok(Err(last_error.context(format!(
                "timed out after {} s waiting for {what}",
                limit.as_secs()
            ))));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_reused_lowest_first_and_run_out_after_sixteen() {
        let numbers = Numbers::default();
        let mut leases: Vec<Lease> = (FIRST_SCREEN..=LAST_SCREEN)
            .map(|_| numbers.lease().unwrap())
            .collect();
        assert_eq!(leases[0].number, 1);
        assert_eq!(leases[15].number, 16);
        assert!(numbers.lease().is_none());

        drop(leases.remove(4));
        drop(leases.remove(1));
        assert_eq!(numbers.lease().unwrap().number, 2);
    }
}
