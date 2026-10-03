//! The Chromium of one screen: how it starts, what it opens, and how it stops.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use computer_protocol::ScreenSize;
use tokio::{
    net::TcpStream,
    process::{Child, Command},
};
use tracing::{info, warn};

use crate::{env, workdir};

const CHROMIUM: &str = "chromium";
const NO_SANDBOX: &str = "--no-sandbox";
const PROFILES_DIR: &str = ".local/share/computer-use/chromium";
const FIRST_DEVTOOLS_PORT: u16 = 9221;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);
const QUIT_TIMEOUT: Duration = Duration::from_secs(8);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_FILES: [&str; 3] = ["SingletonLock", "SingletonCookie", "SingletonSocket"];

/// File types Chromium shows itself, so no other viewer is needed for them.
const BROWSER_EXTENSIONS: [&str; 15] = [
    "html", "htm", "xhtml", "pdf", "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "txt",
    "json", "xml",
];

/// Folder that holds the Chromium profile of screen `number`.
///
/// The profile belongs to the screen number, so a later session on the same number finds its logins.
pub fn profile_dir(home: &Path, number: u8) -> PathBuf {
    home.join(PROFILES_DIR).join(format!("screen-{number}"))
}

/// Port of the `DevTools` endpoint of screen `number`, on the container's loopback only.
pub fn devtools_port(number: u8) -> u16 {
    FIRST_DEVTOOLS_PORT + u16::from(number)
}

/// Command line of a Chromium that starts on screen `number`, opening `url` when given.
///
/// Docker's default seccomp profile blocks the user namespaces the Chromium sandbox needs, so it
/// runs with `--no-sandbox` and the container is the boundary. `--test-type` hides the warning bar
/// that flag causes.
fn command_line(profile: &Path, number: u8, size: ScreenSize, url: Option<&str>) -> Vec<String> {
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-address=127.0.0.1".to_owned(),
        format!("--remote-debugging-port={}", devtools_port(number)),
        NO_SANDBOX.to_owned(),
        "--test-type".to_owned(),
        "--disable-gpu".to_owned(),
        "--enable-unsafe-swiftshader".to_owned(),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
        "--password-store=basic".to_owned(),
        "--disable-session-crashed-bubble".to_owned(),
        "--hide-crash-restore-bubble".to_owned(),
        format!("--window-size={},{}", size.width(), size.height()),
        "--window-position=0,0".to_owned(),
        "--start-maximized".to_owned(),
    ];
    if let Some(url) = url {
        args.push("--".to_owned());
        args.push(url.to_owned());
    }
    args
}

/// What `open_path` was asked to open.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// An http(s) URL.
    Url(String),
    /// A file Chromium shows itself.
    BrowserFile(PathBuf),
    /// A file that opens with its default application.
    DefaultApp(PathBuf),
}

/// Tells a URL from a file path. A relative path starts at `cwd`, `~` is `home`.
///
/// # Errors
///
/// Fails with a message for the agent when the text is empty or a URL of another kind.
pub fn classify(input: &str, cwd: &Path, home: &Path) -> Result<Target, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("give a file path or an http(s) URL to open".to_owned());
    }
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(Target::Url(input.to_owned()));
    }
    if input.contains("://") {
        return Err(format!(
            "{input} is not an http(s) URL. Only http and https URLs and file paths can be opened"
        ));
    }
    let path = workdir::resolve(cwd, home, input);
    let in_browser = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            BROWSER_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        });
    Ok(if in_browser {
        Target::BrowserFile(path)
    } else {
        Target::DefaultApp(path)
    })
}

/// The `file://` URL of an absolute path.
pub fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                url.push(char::from(byte));
            }
            _ => {
                let _ = write!(url, "%{byte:02X}");
            }
        }
    }
    url
}

/// The running Chromium of a screen. The screen owns it and closes it with the screen.
pub struct Browser {
    child: Child,
    profile: PathBuf,
    devtools_port: u16,
}

impl Browser {
    /// Starts Chromium on screen `number` and waits until its `DevTools` endpoint answers.
    pub async fn start(number: u8, size: ScreenSize, url: Option<&str>) -> Result<Self> {
        let profile = profile_dir(&workdir::home_dir(), number);
        let prepared = profile.clone();
        tokio::task::spawn_blocking(move || prepare_profile(&prepared))
            .await
            .context("preparing the browser profile")?
            .context("preparing the browser profile")?;
        let devtools_port = devtools_port(number);
        let child = Command::new(CHROMIUM)
            .args(command_line(&profile, number, size, url))
            .env_clear()
            .envs(env::from_process(Some(number)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting Chromium")?;
        let mut browser = Self {
            child,
            profile,
            devtools_port,
        };
        if let Err(error) = browser.wait_ready().await {
            browser.close().await;
            return Err(error);
        }
        info!(screen = number, devtools_port, "browser started");
        Ok(browser)
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("Chromium exited early with {status}");
            }
            let connect = TcpStream::connect(("127.0.0.1", self.devtools_port));
            if tokio::time::timeout(CONNECT_TIMEOUT, connect)
                .await
                .is_ok_and(|connected| connected.is_ok())
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "Chromium did not open its DevTools port within {} s",
                    READY_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Whether the Chromium process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Opens `url` in a new tab of this Chromium and brings its window forward.
    ///
    /// A second Chromium command on the same profile hands the URL to the running one and exits.
    /// It checks the sandbox before it finds the running one, so it needs the same flag.
    pub async fn open_url(&self, url: &str, number: u8) -> Result<()> {
        let mut child = Command::new(CHROMIUM)
            .arg(format!("--user-data-dir={}", self.profile.display()))
            .args([NO_SANDBOX, "--", url])
            .env_clear()
            .envs(env::from_process(Some(number)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("asking Chromium to open the page")?;
        match tokio::time::timeout(FORWARD_TIMEOUT, child.wait()).await {
            Ok(status) => {
                let status = status.context("waiting for Chromium to take the page")?;
                if !status.success() {
                    bail!("Chromium could not open the page ({status})");
                }
                Ok(())
            }
            Err(_) => bail!(
                "Chromium did not take the page within {} s",
                FORWARD_TIMEOUT.as_secs()
            ),
        }
    }

    /// Asks Chromium to quit so it saves its profile, and kills it when it does not.
    pub async fn close(mut self) {
        ask_to_quit(&self.child);
        let quit = tokio::time::timeout(QUIT_TIMEOUT, self.child.wait()).await;
        if quit.is_err() {
            warn!(
                devtools_port = self.devtools_port,
                "browser did not quit, killing it"
            );
            if let Err(error) = self.child.start_kill() {
                warn!(%error, "could not kill the browser");
            }
            if tokio::time::timeout(KILL_TIMEOUT, self.child.wait())
                .await
                .is_err()
            {
                warn!("browser did not exit after being killed");
            }
        }
        clear_locks(&self.profile);
    }
}

#[cfg(target_os = "linux")]
fn ask_to_quit(child: &Child) {
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: kill takes plain integers and has no memory effects. At worst the process is gone.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[cfg(not(target_os = "linux"))]
fn ask_to_quit(_child: &Child) {}

fn clear_locks(profile: &Path) {
    for name in LOCK_FILES {
        let _ = std::fs::remove_file(profile.join(name));
    }
}

/// Process id in the `SingletonLock` link target, which Chromium writes as `<host>-<pid>`.
fn lock_pid(target: &str) -> Option<u32> {
    target.rsplit_once('-')?.1.parse().ok()
}

/// Whether the process that holds the profile lock is a running Chromium on this profile.
fn lock_holder_alive(profile: &Path) -> bool {
    let Ok(target) = std::fs::read_link(profile.join("SingletonLock")) else {
        return false;
    };
    let Some(pid) = target.to_str().and_then(lock_pid) else {
        return false;
    };
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    String::from_utf8_lossy(&cmdline).contains(&*profile.to_string_lossy())
}

/// Creates the profile folder and removes lock files a killed Chromium left behind.
///
/// # Errors
///
/// Fails when a Chromium that is still running holds the profile.
fn prepare_profile(profile: &Path) -> Result<()> {
    std::fs::create_dir_all(profile)?;
    if lock_holder_alive(profile) {
        bail!(
            "a Chromium from an earlier session is still running on {}, close it or wait for it to exit",
            profile.display()
        );
    }
    clear_locks(profile);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_screen_gets_its_own_profile_in_home_and_its_own_devtools_port() {
        let home = Path::new("/home/computer");
        assert_eq!(
            profile_dir(home, 3),
            Path::new("/home/computer/.local/share/computer-use/chromium/screen-3")
        );
        assert_ne!(profile_dir(home, 1), profile_dir(home, 2));
        assert_eq!(devtools_port(1), 9222);
        assert_eq!(devtools_port(16), 9237);
    }

    #[test]
    fn chromium_listens_on_loopback_with_the_screens_port_and_profile() {
        let size = ScreenSize::parse("1024x768").unwrap();
        let args = command_line(Path::new("/p/screen-2"), 2, size, Some("https://a.test/"));
        for wanted in [
            "--user-data-dir=/p/screen-2",
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=9223",
            "--window-size=1024,768",
        ] {
            assert!(args.iter().any(|arg| arg == wanted), "{wanted} in {args:?}");
        }
        assert_eq!(args[args.len() - 2..], ["--", "https://a.test/"]);
        assert!(!args.iter().any(|arg| arg.contains("dev-shm")));
        assert!(!command_line(Path::new("/p"), 2, size, None).contains(&"--".to_owned()));
    }

    #[test]
    fn urls_and_paths_are_told_apart() {
        let (cwd, home) = (
            Path::new("/home/computer/work"),
            Path::new("/home/computer"),
        );
        let at = |input| classify(input, cwd, home);
        assert_eq!(
            at(" HTTPS://example.com/a b "),
            Ok(Target::Url("HTTPS://example.com/a b".to_owned()))
        );
        assert_eq!(
            at("page.HTML"),
            Ok(Target::BrowserFile(PathBuf::from(
                "/home/computer/work/page.HTML"
            )))
        );
        assert_eq!(
            at("~/notes.odt"),
            Ok(Target::DefaultApp(PathBuf::from(
                "/home/computer/notes.odt"
            )))
        );
        assert!(at("ftp://example.com/a").unwrap_err().contains("ftp://"));
        assert!(at("file:///etc/passwd").is_err());
        assert!(at("  ").is_err());
    }

    #[test]
    fn file_urls_escape_what_would_end_the_path() {
        assert_eq!(
            file_url(Path::new("/home/computer/a b#1?.html")),
            "file:///home/computer/a%20b%231%3F.html"
        );
    }

    #[test]
    fn the_lock_target_names_the_process() {
        assert_eq!(lock_pid("my-host-4242"), Some(4242));
        assert_eq!(lock_pid("nohyphen"), None);
        assert_eq!(lock_pid("host-abc"), None);
    }

    #[test]
    fn preparing_a_profile_removes_stale_locks_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("computerd-profile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["SingletonLock", "SingletonSocket", "Preferences"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        prepare_profile(&dir).unwrap();
        assert!(!dir.join("SingletonLock").exists());
        assert!(!dir.join("SingletonSocket").exists());
        assert!(dir.join("Preferences").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
