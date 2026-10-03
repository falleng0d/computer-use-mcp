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
use computer_protocol::{ActReply, ActRequest, Observation, ScreenSize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::{
    frames::{self, FrameTracker},
    guard::{LoopGuard, Outcome},
    key,
    plan::{self, Input, Step},
    workdir,
    x11::Capturer,
};

pub const FIRST_SCREEN: u8 = 1;
pub const LAST_SCREEN: u8 = 16;

pub const XVNC_FIRST_PORT: u16 = 5900;
const FLUXBOX_INIT: &str = "/etc/computerd/fluxbox-init";
const X_SOCKET_DIR: &str = "/tmp/.X11-unix";
const X_READY_TIMEOUT: Duration = Duration::from_secs(10);
const WM_READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause after each input step so the window manager and applications handle it before the next one.
const STEP_GAP: Duration = Duration::from_millis(30);

/// Why a call on a screen failed.
#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    /// The request cannot be run as given. The screen is fine.
    #[error("{0}")]
    Rejected(String),
    #[error("{0:#}")]
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for ScreenError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl ScreenError {
    fn at_step(self, number: usize, total: usize) -> Self {
        let ran = number - 1;
        match self {
            Self::Rejected(message) => Self::Rejected(format!(
                "action {number} of {total} failed, the {ran} before it already ran: {message}"
            )),
            Self::Failed(error) => Self::Failed(error.context(format!(
                "action {number} of {total} failed, the {ran} before it already ran"
            ))),
        }
    }
}

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
        let xvnc_port = XVNC_FIRST_PORT + u16::from(number);
        let password_file = key::vnc_password_path(&workdir::home_dir());
        let xvnc = Command::new("Xvnc")
            .arg(display(number))
            .args(["-geometry", &size.to_string(), "-depth", "24"])
            .args([
                "-AcceptSetDesktopSize=0",
                "-AlwaysShared",
                "-nolisten",
                "tcp",
            ])
            .args(["-localhost", "-rfbport", &xvnc_port.to_string()])
            .args(["-SecurityTypes", "VncAuth", "-rfbauth"])
            .arg(password_file)
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
    guard: LoopGuard,
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
                    guard: LoopGuard::default(),
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

    /// Runs a batch of actions in order, then takes the closing screenshot when asked.
    ///
    /// Nothing runs when the batch is invalid or repeats a batch that already changed nothing.
    pub async fn act(&mut self, request: ActRequest) -> Result<ActReply, ScreenError> {
        let steps = plan::plan(&request.actions, self.size).map_err(ScreenError::Rejected)?;
        let inputs: Vec<Input> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Input(input) => Some(input.clone()),
                _ => None,
            })
            .collect();
        self.check_keys(inputs).await?;
        let before = self.frame_id().await?;
        self.guard.sync(before);
        if let Some(count) = self.guard.refusal(&request.actions) {
            return Err(ScreenError::Rejected(format!(
                "this exact batch already ran {count} times in a row and the screen did not change, so it was not run again. The latest screenshot is still current. Change your approach: aim at a different target, use the keyboard, or check which window is in front."
            )));
        }
        let total = steps.len();
        for (index, step) in steps.into_iter().enumerate() {
            if let Err(error) = self.run_step(step).await {
                self.guard.reset();
                self.release_held().await;
                return Err(error.at_step(index + 1, total));
            }
        }
        if !request.observe {
            self.guard
                .record(&request.actions, Outcome::Unknown, before);
            return Ok(ActReply {
                actions_run: total,
                observation: None,
            });
        }
        tokio::time::sleep(Duration::from_millis(u64::from(request.settle_ms))).await;
        let observation = self.observe().await?;
        let outcome = if observation.frame_id == before {
            Outcome::Unchanged
        } else {
            Outcome::Changed
        };
        self.guard
            .record(&request.actions, outcome, observation.frame_id);
        Ok(ActReply {
            actions_run: total,
            observation: Some(observation),
        })
    }

    /// Refuses a batch the keyboard cannot type before any of it runs.
    async fn check_keys(&self, inputs: Vec<Input>) -> Result<(), ScreenError> {
        let source = Arc::clone(&self.source);
        tokio::task::spawn_blocking(move || {
            source
                .lock()
                .expect("the capture lock is only held inside spawn_blocking")
                .capturer
                .check_keys(&inputs)
        })
        .await
        .context("checking the keyboard")?
        .map_err(|error| ScreenError::Rejected(format!("{error:#}")))
    }

    /// Releases mouse buttons a failed batch left pressed.
    async fn release_held(&self) {
        let source = Arc::clone(&self.source);
        let released = tokio::task::spawn_blocking(move || {
            source
                .lock()
                .expect("the capture lock is only held inside spawn_blocking")
                .capturer
                .release_held()
        })
        .await;
        if let Ok(Err(error)) | Err(error) = released.map_err(anyhow::Error::from) {
            warn!(error = %format!("{error:#}"), "could not release held buttons");
        }
    }

    /// Id of the frame the screen shows now, without taking an image.
    async fn frame_id(&self) -> Result<u64> {
        let source = Arc::clone(&self.source);
        tokio::task::spawn_blocking(move || {
            let mut guard = source
                .lock()
                .expect("the capture lock is only held inside spawn_blocking");
            let Source { capturer, frames } = &mut *guard;
            Ok(frames.observe(capturer.take_damage()?).id)
        })
        .await
        .context("checking the screen")?
    }

    async fn run_step(&self, step: Step) -> Result<(), ScreenError> {
        let source = Arc::clone(&self.source);
        match step {
            Step::Wait(duration) => {
                tokio::time::sleep(duration).await;
                return Ok(());
            }
            Step::Input(input) => {
                tokio::task::spawn_blocking(move || {
                    let guard = source
                        .lock()
                        .expect("the capture lock is only held inside spawn_blocking");
                    let performed = guard.capturer.perform(&input);
                    if performed.is_err() {
                        let _ = guard.capturer.release_held();
                    }
                    performed
                })
                .await
                .context("running the action")??;
            }
            Step::Focus(application) => {
                let target = application.clone();
                let open: Option<Vec<String>> = tokio::task::spawn_blocking(move || {
                    let guard = source
                        .lock()
                        .expect("the capture lock is only held inside spawn_blocking");
                    let windows = guard.capturer.windows()?;
                    match plan::pick_window(&windows, &target) {
                        Some(window) => guard.capturer.activate(window.id).map(|()| None),
                        None => Ok(Some(
                            windows.iter().map(plan::WindowInfo::describe).collect(),
                        )),
                    }
                })
                .await
                .context("focusing the window")??;
                if let Some(open) = open {
                    return Err(no_window(&application, &open));
                }
            }
        }
        tokio::time::sleep(STEP_GAP).await;
        Ok(())
    }

    /// Stops the screen's processes. The screen number frees when `self` drops.
    pub async fn close(self) {
        info!(screen = self.number, "closing screen");
        self.processes.stop().await;
    }
}

fn no_window(application: &str, open: &[String]) -> ScreenError {
    let listing = if open.is_empty() {
        "No windows are open.".to_owned()
    } else {
        format!("Open windows: {}.", open.join(", "))
    };
    ScreenError::Rejected(format!(
        "no open window matches {application:?}. {listing} Launching applications is not available yet, so focus only raises windows that are already open."
    ))
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
