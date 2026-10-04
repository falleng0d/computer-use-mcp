//! Opens the viewer on the user's machine when an agent's screen opens.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use computer_protocol::{viewer_screen_link, vnc_password_file};
use tokio::{sync::Mutex, task::JoinSet, time::Instant};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{client::Client, computer::Endpoint, settings};

const VIEWER_PROGRAM: &str = "vncviewer.exe";
const PASSWD_FILE_PREFIX: &str = "computer-use-";
const PASSWD_FILE_SUFFIX: &str = ".vncpasswd";
const STALE_AFTER: Duration = Duration::from_secs(60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long after opening a tab the next screens are shown there instead of opening another tab.
const TAB_GRACE: Duration = Duration::from_secs(10);
const PAGE_POLL: Duration = Duration::from_millis(500);
/// How long the VNC viewer has to read its password file.
const PASSWD_FILE_LIFETIME: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Where a newly opened screen shows up for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Browser,
    Vnc,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Windows,
    Mac,
    Other,
}

impl Os {
    const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Nothing,
    Show,
    Browser,
    MacVnc,
    WindowsVnc,
}

#[derive(Debug, PartialEq, Eq)]
struct Decision {
    step: Step,
    /// Why `vnc` mode opens the browser instead.
    fallback: Option<&'static str>,
}

/// Picks what to open. A page that is open, or a tab opened moments ago, gets the screen shown in place of a new tab.
fn decide(mode: Mode, os: Os, pages: usize, tab_recent: bool, viewer_found: bool) -> Decision {
    let browser = || {
        if pages > 0 || tab_recent {
            Step::Show
        } else {
            Step::Browser
        }
    };
    let (step, fallback) = match (mode, os) {
        (Mode::None, _) => (Step::Nothing, None),
        (Mode::Browser, _) => (browser(), None),
        (Mode::Vnc, Os::Mac) => (Step::MacVnc, None),
        (Mode::Vnc, Os::Windows) if viewer_found => (Step::WindowsVnc, None),
        (Mode::Vnc, Os::Windows) => (
            browser(),
            Some("no TigerVNC viewer found, set COMPUTER_USE_VNC_VIEWER to its path"),
        ),
        (Mode::Vnc, Os::Other) => (browser(), Some("vnc mode needs macOS or Windows")),
    };
    Decision { step, fallback }
}

fn mac_vnc_url(key: &str, port: u16) -> String {
    format!("vnc://:{key}@127.0.0.1:{port}")
}

/// Program and arguments that open `url` in the default browser.
///
/// Windows goes through the URL handler directly, because `cmd /c start` breaks URLs with `&`.
fn browser_command(os: Os, url: &str) -> (OsString, Vec<OsString>) {
    match os {
        Os::Windows => (
            "rundll32".into(),
            vec!["url.dll,FileProtocolHandler".into(), url.into()],
        ),
        Os::Mac => ("open".into(), vec![url.into()]),
        Os::Other => ("xdg-open".into(), vec![url.into()]),
    }
}

/// Arguments for `vncviewer`: password file, then the host and port (`::` marks a port, not a display number).
fn vncviewer_args(passwd_file: &Path, port: u16) -> Vec<OsString> {
    vec![
        "-passwd".into(),
        passwd_file.into(),
        format!("127.0.0.1::{port}").into(),
    ]
}

/// Where to look for the viewer, first match wins: the setting, then `PATH`, then the install folders of `TigerVNC`.
fn viewer_candidates(
    setting: Option<&str>,
    path_dirs: &[PathBuf],
    program_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let set = setting.map(str::trim).filter(|text| !text.is_empty());
    set.map(PathBuf::from)
        .into_iter()
        .chain(path_dirs.iter().map(|dir| dir.join(VIEWER_PROGRAM)))
        .chain(
            program_dirs
                .iter()
                .flat_map(|dir| [dir.join("TigerVNC"), dir.join("TigerVNC Viewer")])
                .map(|dir| dir.join(VIEWER_PROGRAM)),
        )
        .collect()
}

/// The viewer program, or `None`. A setting is used only when it names a file.
fn find_viewer() -> Option<PathBuf> {
    let setting = settings::vnc_viewer();
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    let program_dirs: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    let candidates = viewer_candidates(setting.as_deref(), &path_dirs, &program_dirs);
    if let Some(set) = setting.as_deref().filter(|text| !text.trim().is_empty()) {
        let path = candidates.into_iter().next()?;
        if path.is_file() {
            return Some(path);
        }
        warn!(
            setting = settings::VNC_VIEWER_ENV,
            value = set,
            "the setting is not a file"
        );
        return None;
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn is_password_file(name: &str) -> bool {
    name.starts_with(PASSWD_FILE_PREFIX) && name.ends_with(PASSWD_FILE_SUFFIX)
}

/// Removes password files that an earlier server left in the temp folder, such as after a crash.
pub fn remove_stale_password_files() {
    remove_stale_in(&std::env::temp_dir(), STALE_AFTER);
}

fn remove_stale_in(dir: &Path, older_than: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > older_than));
        if old && name.to_str().is_some_and(is_password_file) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

struct Shared {
    /// When the last tab was opened. Held while deciding and opening, so concurrent screens open one tab.
    last_tab: Mutex<Option<Instant>>,
    /// Password files not yet removed.
    files: StdMutex<Vec<PathBuf>>,
}

impl Shared {
    fn forget(&self, file: &Path) {
        self.files
            .lock()
            .expect("the file list lock is only held to add or remove a path")
            .retain(|known| known != file);
    }
}

/// Opens the viewer for new screens in tasks of its own, so a tool call never waits for it.
pub struct Opener {
    mode: Mode,
    shared: Arc<Shared>,
    tasks: StdMutex<JoinSet<()>>,
}

impl Opener {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            shared: Arc::new(Shared {
                last_tab: Mutex::new(None),
                files: StdMutex::new(Vec::new()),
            }),
            tasks: StdMutex::new(JoinSet::new()),
        }
    }

    /// Starts opening the viewer on `screen`. Problems are logged, never returned.
    pub fn screen_opened(&self, endpoint: Endpoint, screen: u8) {
        if self.mode == Mode::None {
            return;
        }
        let (mode, shared) = (self.mode, Arc::clone(&self.shared));
        let mut tasks = self
            .tasks
            .lock()
            .expect("the task set lock is only held to add or drain tasks");
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let opened =
                tokio::time::timeout(OPEN_TIMEOUT, open(mode, &shared, &endpoint, screen)).await;
            let file = match opened {
                Ok(Ok(file)) => file,
                Ok(Err(error)) => {
                    warn!(screen, error = %format!("{error:#}"), "could not open the viewer");
                    None
                }
                Err(_) => {
                    warn!(screen, "opening the viewer timed out");
                    None
                }
            };
            if let Some(file) = file {
                tokio::time::sleep(PASSWD_FILE_LIFETIME).await;
                remove_file(&shared, &file);
            }
        });
    }

    /// Cancels pending openers and removes password files.
    pub async fn shutdown(&self) {
        let mut tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .expect("the task set lock is only held to add or drain tasks"),
        );
        tasks.abort_all();
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        let left = std::mem::take(
            &mut *self
                .shared
                .files
                .lock()
                .expect("the file list lock is only held to add or remove a path"),
        );
        for file in left {
            let _ = std::fs::remove_file(file);
        }
    }
}

fn remove_file(shared: &Shared, file: &Path) {
    if let Err(error) = std::fs::remove_file(file) {
        warn!(%error, "could not remove the VNC password file");
    }
    shared.forget(file);
}

/// Opens the viewer once. Returns a password file the caller removes later.
async fn open(
    mode: Mode,
    shared: &Shared,
    endpoint: &Endpoint,
    screen: u8,
) -> Result<Option<PathBuf>> {
    let os = Os::current();
    let client = Client::new(endpoint)?;
    let Some(port) = endpoint.viewer_port else {
        bail!(
            "this computer predates the viewer, remove it with `docker rm` so it is created again"
        );
    };
    let viewer = if mode == Mode::Vnc && os == Os::Windows {
        tokio::task::spawn_blocking(find_viewer)
            .await
            .context("looking for a VNC viewer")?
    } else {
        None
    };
    let mut last_tab = shared.last_tab.lock().await;
    let mut info = client.viewer().await?;
    let tab_recent = last_tab.is_some_and(|at| at.elapsed() < TAB_GRACE);
    let decision = decide(mode, os, info.pages, tab_recent, viewer.is_some());
    if let Some(reason) = decision.fallback {
        warn!(reason, "opening the browser instead of a VNC viewer");
    }
    let mut step = decision.step;
    if step == Step::Show {
        let started = Instant::now();
        while info.pages == 0 && started.elapsed() < TAB_GRACE {
            tokio::time::sleep(PAGE_POLL).await;
            info = client.viewer().await?;
        }
        if info.pages == 0 {
            step = Step::Browser;
        }
    }
    match step {
        Step::Nothing => {}
        Step::Show => {
            let reply = client.show_screen(screen).await?;
            info!(
                screen,
                pages = reply.pages,
                "showing the screen in the open viewer page"
            );
        }
        Step::Browser => {
            info!(screen, port, "opening the viewer page in the browser");
            let (program, args) = browser_command(os, &viewer_screen_link(port, &info.key, screen));
            run(&program, &args).await?;
            *last_tab = Some(Instant::now());
        }
        Step::MacVnc => {
            info!(screen, "opening the screen in Screen Sharing");
            let url = mac_vnc_url(&info.key, port + u16::from(screen));
            run(&OsString::from("open"), &[url.into()]).await?;
        }
        Step::WindowsVnc => {
            let program = viewer.context("the VNC viewer disappeared")?;
            let file = write_password_file(shared, &info.key).await?;
            info!(screen, viewer = %program.display(), "opening the screen in the VNC viewer");
            let args = vncviewer_args(&file, port + u16::from(screen));
            if let Err(error) = start(&program.into_os_string(), &args) {
                remove_file(shared, &file);
                return Err(error);
            }
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn command(program: &OsString, args: &[OsString]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Runs a short command and waits for it, killing it when it takes too long.
async fn run(program: &OsString, args: &[OsString]) -> Result<()> {
    let name = program.to_string_lossy().into_owned();
    let mut child = command(program, args)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting `{name}`"))?;
    let status = tokio::time::timeout(COMMAND_TIMEOUT, child.wait())
        .await
        .with_context(|| format!("`{name}` did not finish in {} s", COMMAND_TIMEOUT.as_secs()))?
        .with_context(|| format!("waiting for `{name}`"))?;
    if !status.success() {
        bail!("`{name}` failed with {status}");
    }
    Ok(())
}

/// Starts a program that stays open, such as a VNC viewer window. Tokio reaps it when it exits.
fn start(program: &OsString, args: &[OsString]) -> Result<()> {
    let name = program.to_string_lossy().into_owned();
    command(program, args)
        .spawn()
        .map(drop)
        .with_context(|| format!("starting `{name}`"))
}

/// Writes the VNC password file in the temp folder, readable by this user only.
async fn write_password_file(shared: &Shared, key: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "{PASSWD_FILE_PREFIX}{}{PASSWD_FILE_SUFFIX}",
        Uuid::new_v4().simple()
    ));
    let bytes = vnc_password_file(key);
    let target = path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&target)?.write_all(&bytes)
    })
    .await
    .context("writing the VNC password file")?
    .context("writing the VNC password file")?;
    shared
        .files
        .lock()
        .expect("the file list lock is only held to add or remove a path")
        .push(path.clone());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_mode_opens_a_tab_only_when_no_page_is_there_or_coming() {
        let step = |pages, recent| decide(Mode::Browser, Os::Windows, pages, recent, false).step;
        assert_eq!(step(0, false), Step::Browser);
        assert_eq!(step(1, false), Step::Show);
        assert_eq!(step(0, true), Step::Show);
        assert_eq!(
            decide(Mode::None, Os::Mac, 0, false, true).step,
            Step::Nothing
        );
    }

    #[test]
    fn vnc_mode_uses_screen_sharing_on_macos_and_a_found_viewer_on_windows() {
        let step = |os, found| decide(Mode::Vnc, os, 0, false, found);
        assert_eq!(
            step(Os::Mac, false),
            Decision {
                step: Step::MacVnc,
                fallback: None
            }
        );
        assert_eq!(
            step(Os::Windows, true),
            Decision {
                step: Step::WindowsVnc,
                fallback: None
            }
        );
    }

    #[test]
    fn vnc_mode_without_a_viewer_falls_back_to_the_browser_with_a_reason() {
        let missing = decide(Mode::Vnc, Os::Windows, 0, false, false);
        assert_eq!(missing.step, Step::Browser);
        assert!(
            missing
                .fallback
                .unwrap()
                .contains("COMPUTER_USE_VNC_VIEWER")
        );
        let shown = decide(Mode::Vnc, Os::Other, 2, false, false);
        assert_eq!(shown.step, Step::Show);
        assert!(shown.fallback.is_some());
    }

    #[test]
    fn commands_keep_the_whole_url_in_one_argument() {
        let url = "http://127.0.0.1:20900/#key=abcd2345&screen=3";
        let (program, args) = browser_command(Os::Windows, url);
        assert_eq!(program, "rundll32");
        assert_eq!(args, ["url.dll,FileProtocolHandler", url]);
        let (program, args) = browser_command(Os::Mac, url);
        assert_eq!((program, args), ("open".into(), vec![url.into()]));
        assert_eq!(
            mac_vnc_url("abcd2345", 20903),
            "vnc://:abcd2345@127.0.0.1:20903"
        );
    }

    #[test]
    fn the_viewer_gets_the_password_file_and_an_explicit_port() {
        let args = vncviewer_args(Path::new("pw"), 20903);
        assert_eq!(args, ["-passwd", "pw", "127.0.0.1::20903"]);
    }

    #[test]
    fn viewer_search_prefers_the_setting_then_path_then_install_folders() {
        let found = viewer_candidates(
            Some(" C:/tools/vnc.exe "),
            &[PathBuf::from("/bin")],
            &[PathBuf::from("/pf")],
        );
        assert_eq!(
            found,
            [
                PathBuf::from("C:/tools/vnc.exe"),
                PathBuf::from("/bin/vncviewer.exe"),
                PathBuf::from("/pf/TigerVNC/vncviewer.exe"),
                PathBuf::from("/pf/TigerVNC Viewer/vncviewer.exe"),
            ]
        );
        assert_eq!(
            viewer_candidates(Some(" "), &[], &[]),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn only_old_password_files_are_removed_from_the_temp_folder() {
        let dir = std::env::temp_dir().join(format!("open-test-{}", Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        let make = |name: &str, age: Duration| {
            let path = dir.join(name);
            let file = std::fs::File::create(&path).unwrap();
            file.set_modified(std::time::SystemTime::now() - age)
                .unwrap();
            path
        };
        let old = make("computer-use-a.vncpasswd", Duration::from_secs(300));
        let new = make("computer-use-b.vncpasswd", Duration::ZERO);
        let other = make("computer-use-c.txt", Duration::from_secs(300));
        remove_stale_in(&dir, Duration::from_secs(60));
        assert!(!old.exists());
        assert!(new.exists());
        assert!(other.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
