use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
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
    apps::{self, App},
    browser::{self, Browser, Target},
    env,
    frames::{self, FrameTracker},
    guard::{LoopGuard, Outcome},
    key,
    numbers::{FIRST_SCREEN, LAST_SCREEN, Lease},
    plan::{self, Input, Step, WindowInfo},
    workdir,
    x11::Capturer,
};

pub(crate) const XVNC_FIRST_PORT: u16 = 5900;
const FLUXBOX_INIT: &str = "/etc/computerd/fluxbox-init";
const X_SOCKET_DIR: &str = "/tmp/.X11-unix";
const X_READY_TIMEOUT: Duration = Duration::from_secs(10);
const WM_READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for a window to appear after something is started.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(5);
const WINDOW_POLL: Duration = Duration::from_millis(100);
/// Pause after a page is handed to the browser, so it can start to load before the screenshot.
const PAGE_SETTLE: Duration = Duration::from_millis(2000);
/// Pause after an application starts, so its first window can paint before the screenshot.
const APP_SETTLE: Duration = Duration::from_millis(700);
const BROWSER_CLASS: &str = "chromium";
const TERMINAL_CLASS: &str = "xterm";
const OPEN_COMMAND: &str = "xdg-open";

/// Pause after each input step so the window manager and applications handle it before the next one.
const STEP_GAP: Duration = Duration::from_millis(30);

/// Why a call on a screen failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ScreenError {
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

/// Removes lock and socket files that a killed X server left behind.
pub(crate) fn clean_stale_x_files() {
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
            .env_clear()
            .envs(env::from_process(None))
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
            .env_clear()
            .envs(env::from_process(Some(self.number)))
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
pub(crate) struct Screen {
    _lease: Lease,
    number: u8,
    size: ScreenSize,
    processes: Processes,
    source: Arc<Mutex<Source>>,
    guard: LoopGuard,
    /// Chromium of this screen, started the first time something needs a browser.
    browser: Option<Browser>,
    /// Applications started on this screen that may still run.
    apps: Vec<Child>,
}

/// Everything a capture touches, locked together inside `spawn_blocking`.
struct Source {
    capturer: Capturer,
    frames: FrameTracker,
}

impl Screen {
    /// Starts `Xvnc` and Fluxbox on the leased number and waits until both are usable.
    pub(crate) async fn open(lease: Lease, size: ScreenSize) -> Result<Self> {
        let number = lease.number();
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
                    browser: None,
                    apps: Vec::new(),
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
    pub(crate) fn check_alive(&mut self) -> Result<()> {
        self.processes.check_alive()
    }

    pub(crate) fn number(&self) -> u8 {
        self.number
    }

    /// Captures the screen. The image is left out when the session already has this frame.
    pub(crate) async fn observe(&mut self) -> Result<Observation> {
        let source = Arc::clone(&self.source);
        let size = self.size;
        tokio::task::spawn_blocking(move || capture(&source, size))
            .await
            .context("running the capture")?
    }

    /// Runs a batch of actions in order, then takes the closing screenshot when asked.
    ///
    /// Nothing runs when the batch is invalid or repeats a batch that already changed nothing.
    pub(crate) async fn act(
        &mut self,
        request: ActRequest,
        cwd: &Path,
    ) -> Result<ActReply, ScreenError> {
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
            if let Err(error) = self.run_step(step, cwd).await {
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
                opened_screen: None,
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
            opened_screen: None,
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

    async fn run_step(&mut self, step: Step, cwd: &Path) -> Result<(), ScreenError> {
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
            Step::Focus { application, uri } => {
                self.focus_or_launch(&application, uri.as_deref(), cwd)
                    .await?;
            }
        }
        tokio::time::sleep(STEP_GAP).await;
        Ok(())
    }

    /// Closes the browser, kills the applications, and stops the screen's processes.
    /// The screen number frees when `self` drops.
    pub(crate) async fn close(mut self) {
        info!(screen = self.number, "closing screen");
        if let Some(browser) = self.browser.take() {
            browser.close().await;
        }
        for mut app in std::mem::take(&mut self.apps) {
            apps::kill_group(&mut app);
            let _ = tokio::time::timeout(STOP_TIMEOUT, app.wait()).await;
        }
        self.processes.stop().await;
    }

    /// Windows the window manager lists now.
    async fn windows(&self) -> Result<Vec<WindowInfo>> {
        let source = Arc::clone(&self.source);
        tokio::task::spawn_blocking(move || {
            source
                .lock()
                .expect("the capture lock is only held inside spawn_blocking")
                .capturer
                .windows()
        })
        .await
        .context("listing the windows")?
    }

    async fn raise(&self, window: u32) -> Result<()> {
        let source = Arc::clone(&self.source);
        tokio::task::spawn_blocking(move || {
            source
                .lock()
                .expect("the capture lock is only held inside spawn_blocking")
                .capturer
                .activate(window)
        })
        .await
        .context("raising the window")?
    }

    /// Opens a file or an http(s) URL, then returns a screenshot.
    ///
    /// Pages and files Chromium shows go to the screen's browser. Other files open with their default application.
    pub(crate) async fn open_path(
        &mut self,
        input: &str,
        cwd: &Path,
    ) -> Result<Observation, ScreenError> {
        let target =
            browser::classify(input, cwd, &workdir::home_dir()).map_err(ScreenError::Rejected)?;
        let settle = match target {
            Target::Url(url) => {
                self.show_resolved(Some(&url)).await?;
                PAGE_SETTLE
            }
            Target::BrowserFile(path) => {
                check_file(&path).await?;
                self.show_resolved(Some(&browser::file_url(&path))).await?;
                PAGE_SETTLE
            }
            Target::DefaultApp(path) => {
                check_file(&path).await?;
                let argv = vec![OPEN_COMMAND.to_owned(), path.display().to_string()];
                self.start_app("opening the file", argv, cwd).await?;
                APP_SETTLE
            }
        };
        tokio::time::sleep(settle).await;
        Ok(self.observe().await?)
    }

    /// Starts or raises an application, then returns a screenshot.
    pub(crate) async fn launch_app(
        &mut self,
        application: &str,
        uri: Option<&str>,
        cwd: &Path,
    ) -> Result<Observation, ScreenError> {
        self.focus_or_launch(application, uri, cwd).await?;
        let settle = if uri.is_some() {
            PAGE_SETTLE
        } else {
            APP_SETTLE
        };
        tokio::time::sleep(settle).await;
        Ok(self.observe().await?)
    }

    /// Raises the window an application name matches. Starts the application when no window
    /// matches, or when there is something to open in it.
    async fn focus_or_launch(
        &mut self,
        application: &str,
        uri: Option<&str>,
        cwd: &Path,
    ) -> Result<(), ScreenError> {
        let home = workdir::home_dir();
        let name = application.to_owned();
        let wanted_uri = uri.map(str::to_owned);
        let app = tokio::task::spawn_blocking(move || {
            apps::find_app(&name, wanted_uri.as_deref(), &home)
        })
        .await
        .context("looking for the application")?;
        let windows = self.windows().await?;
        let in_window = |class: &str| plan::pick_window(&windows, class).map(|window| window.id);
        match app {
            Some(App::Browser) => self.show_in_browser(uri, cwd).await,
            Some(App::Terminal) => {
                let existing = in_window(TERMINAL_CLASS);
                self.raise_or_start(existing, "Terminal", apps::terminal_argv(), cwd)
                    .await
            }
            Some(App::Command { label, argv }) => {
                if uri.is_some_and(|uri| uri.starts_with('-')) {
                    return Err(ScreenError::Rejected(format!(
                        "the uri {uri:?} starts with a dash, so {label} would read it as an option"
                    )));
                }
                let program = argv[0].rsplit('/').next().unwrap_or_default();
                let existing = if uri.is_some() {
                    None
                } else {
                    in_window(application).or_else(|| in_window(program))
                };
                self.raise_or_start(existing, &label, argv, cwd).await
            }
            None => match in_window(application) {
                Some(window) => Ok(self.raise(window).await?),
                None => Err(no_window(
                    application,
                    &windows.iter().map(WindowInfo::describe).collect::<Vec<_>>(),
                )),
            },
        }
    }

    async fn raise_or_start(
        &mut self,
        existing: Option<u32>,
        label: &str,
        argv: Vec<String>,
        cwd: &Path,
    ) -> Result<(), ScreenError> {
        match existing {
            Some(window) => Ok(self.raise(window).await?),
            None => self.start_app(label, argv, cwd).await,
        }
    }

    /// Opens a page in the screen's browser, or just raises it without one. `input` is an http(s)
    /// URL or a file the browser shows, and a relative path starts at `cwd`.
    pub(crate) async fn show_in_browser(
        &mut self,
        input: Option<&str>,
        cwd: &Path,
    ) -> Result<(), ScreenError> {
        let url = match input {
            None => None,
            Some(input) => {
                let target = browser::classify(input, cwd, &workdir::home_dir())
                    .map_err(ScreenError::Rejected)?;
                Some(match target {
                    Target::Url(url) => url,
                    Target::BrowserFile(path) => {
                        check_file(&path).await?;
                        browser::file_url(&path)
                    }
                    Target::DefaultApp(_) => {
                        return Err(ScreenError::Rejected(format!(
                            "{input} is not a page or a file the browser shows. Give an http(s) URL or an HTML, PDF, image, text, JSON, or XML file"
                        )));
                    }
                })
            }
        };
        self.show_resolved(url.as_deref()).await
    }

    /// Opens a checked `url` in the screen's browser, starting the browser when it is not running.
    async fn show_resolved(&mut self, url: Option<&str>) -> Result<(), ScreenError> {
        let number = self.number;
        if let Some(browser) = self.browser.as_mut()
            && browser.is_running()
        {
            if let Some(url) = url {
                browser
                    .open_url(url, number)
                    .await
                    .map_err(ScreenError::Failed)?;
            }
            let windows = self.windows().await?;
            if let Some(window) = plan::pick_window(&windows, BROWSER_CLASS) {
                self.raise(window.id).await?;
            }
            return Ok(());
        }
        if let Some(gone) = self.browser.take() {
            gone.close().await;
        }
        let before = self.window_ids().await?;
        let browser = Browser::start(number, self.size, url)
            .await
            .map_err(ScreenError::Failed)?;
        self.browser = Some(browser);
        self.wait_for_window(&before, None).await?;
        Ok(())
    }

    async fn window_ids(&self) -> Result<BTreeSet<u32>> {
        Ok(self
            .windows()
            .await?
            .iter()
            .map(|window| window.id)
            .collect())
    }

    /// Waits until a window that is not in `before` exists, up to [`WINDOW_TIMEOUT`].
    /// Returns the exit status of `child` when it fails before a window shows up.
    async fn wait_for_window(
        &self,
        before: &BTreeSet<u32>,
        mut child: Option<&mut Child>,
    ) -> Result<Option<std::process::ExitStatus>, ScreenError> {
        let deadline = tokio::time::Instant::now() + WINDOW_TIMEOUT;
        loop {
            if let Some(child) = child.as_deref_mut()
                && let Some(status) = child.try_wait().context("checking the new process")?
                && !status.success()
            {
                return Ok(Some(status));
            }
            let shown = self
                .windows()
                .await?
                .iter()
                .any(|window| !before.contains(&window.id));
            if shown || tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(WINDOW_POLL).await;
        }
    }

    /// Starts a program on this screen and waits briefly for its window.
    async fn start_app(
        &mut self,
        label: &str,
        argv: Vec<String>,
        cwd: &Path,
    ) -> Result<(), ScreenError> {
        self.apps
            .retain_mut(|app| matches!(app.try_wait(), Ok(None)));
        let before = self.window_ids().await?;
        let mut child = apps::spawn(&argv, cwd, self.number).map_err(|error| {
            ScreenError::Rejected(format!("cannot start {label} ({}): {error}", argv[0]))
        })?;
        if let Some(status) = self.wait_for_window(&before, Some(&mut child)).await? {
            let hint = if argv[0] == OPEN_COMMAND {
                ". No installed application opens this kind of file"
            } else {
                ""
            };
            return Err(ScreenError::Rejected(format!(
                "{label} ({}) exited with {status} before opening a window{hint}",
                argv[0]
            )));
        }
        if matches!(child.try_wait(), Ok(None)) {
            self.apps.push(child);
        }
        Ok(())
    }
}

async fn check_file(path: &Path) -> Result<(), ScreenError> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Ok(()),
        Err(error) => Err(ScreenError::Rejected(format!(
            "cannot open {}: {error}",
            path.display()
        ))),
    }
}

fn no_window(application: &str, open: &[String]) -> ScreenError {
    let listing = if open.is_empty() {
        "No windows are open.".to_owned()
    } else {
        format!("Open windows: {}.", open.join(", "))
    };
    ScreenError::Rejected(format!(
        "no open window matches {application:?} and no installed application has that name. {listing}"
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
        opened_screen: None,
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
