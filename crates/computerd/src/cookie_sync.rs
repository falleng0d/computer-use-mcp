//! Keeps the cookies of every running browser in step through one jar saved in home.
//!
//! `DevTools` has no cookie-change event, so every pass reads each browser's cookies, folds what
//! changed into the jar, and writes the jar's differences into the other browsers.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    cookies::{self, Jar, Pending, Snapshot},
    devtools::Connection,
};

const JAR_FILE: &str = ".local/share/computer-use/cookies.json";
const SYNC_INTERVAL: Duration = Duration::from_secs(2);
const SAVE_INTERVAL: Duration = Duration::from_secs(5);
const BROWSER_TIMEOUT: Duration = Duration::from_secs(12);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);
const FINAL_SYNC_TIMEOUT: Duration = Duration::from_secs(4);
/// Reads in a row that may fail before a browser is no longer tracked.
const MAX_FAILURES: u32 = 30;

static SHARED: OnceLock<CookieSync> = OnceLock::new();

/// The jar and the browsers that share it.
pub struct CookieSync {
    state: Mutex<State>,
}

struct State {
    jar: Jar,
    path: PathBuf,
    browsers: BTreeMap<u8, Tracked>,
    last_save: Instant,
}

struct Tracked {
    port: u16,
    snapshot: Snapshot,
    failures: u32,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// Where the jar lives in `home`.
pub fn jar_path(home: &Path) -> PathBuf {
    home.join(JAR_FILE)
}

/// Loads the jar and makes the sync available to browsers. Calling it twice keeps the first.
pub async fn install(path: PathBuf) -> &'static CookieSync {
    let loaded = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || load(&path)).await
    };
    let mut jar = loaded.unwrap_or_else(|error| {
        warn!(%error, "could not read the cookie jar, starting empty");
        Jar::default()
    });
    jar.prune(now());
    SHARED.get_or_init(|| CookieSync {
        state: Mutex::new(State {
            jar,
            path,
            browsers: BTreeMap::new(),
            last_save: Instant::now(),
        }),
    })
}

/// The installed sync, when `computerd` runs with one.
pub fn shared() -> Option<&'static CookieSync> {
    SHARED.get()
}

fn load(path: &Path) -> Jar {
    match std::fs::read_to_string(path) {
        Ok(text) => Jar::from_json(&text).unwrap_or_else(|error| {
            warn!(%error, "the cookie jar is unreadable, starting empty");
            Jar::default()
        }),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(%error, "could not read the cookie jar, starting empty");
            }
            Jar::default()
        }
    }
}

/// Writes `text` to `path` through a temporary file, readable by the owner only.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, path)
}

async fn read_cookies(port: u16) -> Result<Snapshot> {
    let read = async {
        let mut connection = Connection::browser(port).await?;
        connection.get_cookies().await
    };
    let cookies = tokio::time::timeout(BROWSER_TIMEOUT, read)
        .await
        .context("reading cookies timed out")??;
    Ok(cookies::snapshot(cookies))
}

/// Writes what the jar holds that the browser lacks. Returns how many cookies went in.
async fn write_cookies(port: u16, pending: &Pending) -> Result<usize> {
    let params: Vec<_> = pending
        .set
        .iter()
        .map(cookies::to_param)
        .chain(pending.delete.iter().map(cookies::to_delete_param))
        .collect();
    let write = async {
        let mut connection = Connection::browser(port).await?;
        if connection.set_cookies(params.clone()).await.is_ok() {
            return Ok(params.len());
        }
        let mut written = 0;
        for param in params {
            if connection.set_cookies(vec![param]).await.is_ok() {
                written += 1;
            }
        }
        anyhow::Ok(written)
    };
    tokio::time::timeout(BROWSER_TIMEOUT, write)
        .await
        .context("writing cookies timed out")?
}

impl CookieSync {
    /// Starts sharing with the browser of screen `number`, giving it every cookie in the jar.
    ///
    /// Never fails. The browser still works when `DevTools` does not answer, it just shares nothing.
    pub async fn attach(&self, number: u8, port: u16) {
        let mut state = self.state.lock().await;
        let result = tokio::time::timeout(ATTACH_TIMEOUT, load_jar_into(&state.jar, port)).await;
        let snapshot = match result {
            Ok(Ok((snapshot, loaded))) => {
                info!(
                    screen = number,
                    cookies = loaded,
                    "browser started with the shared cookies"
                );
                snapshot
            }
            Ok(Err(error)) => {
                warn!(screen = number, %error, "could not load the shared cookies into the browser");
                Snapshot::new()
            }
            Err(_) => {
                warn!(screen = number, "loading the shared cookies timed out");
                Snapshot::new()
            }
        };
        state.browsers.insert(
            number,
            Tracked {
                port,
                snapshot,
                failures: 0,
            },
        );
    }

    /// Stops sharing with the browser of screen `number` after one last read of its cookies.
    pub async fn detach(&self, number: u8) {
        let mut state = self.state.lock().await;
        let Some(tracked) = state.browsers.remove(&number) else {
            return;
        };
        let read = tokio::time::timeout(FINAL_SYNC_TIMEOUT, read_cookies(tracked.port)).await;
        match read {
            Ok(Ok(current)) => {
                let changes = cookies::diff(&tracked.snapshot, &current, now());
                state.jar.apply(&changes, now());
                debug!(
                    screen = number,
                    changed = changes.len(),
                    "final cookie read"
                );
            }
            Ok(Err(error)) => warn!(screen = number, %error, "final cookie read failed"),
            Err(_) => warn!(screen = number, "final cookie read timed out"),
        }
        state.save(true).await;
    }

    /// Runs a pass every [`SYNC_INTERVAL`] until `stop` is cancelled, then saves the jar.
    pub async fn run(&self, stop: CancellationToken) {
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                () = tokio::time::sleep(SYNC_INTERVAL) => self.pass().await,
            }
        }
        self.state.lock().await.save(true).await;
    }

    async fn pass(&self) {
        let mut state = self.state.lock().await;
        let numbers: Vec<u8> = state.browsers.keys().copied().collect();
        let mut read = Vec::new();
        for number in &numbers {
            let Some(port) = state.browsers.get(number).map(|tracked| tracked.port) else {
                continue;
            };
            match read_cookies(port).await {
                Ok(current) => {
                    let at = now();
                    let tracked = state.browsers.get_mut(number).expect("tracked above");
                    tracked.failures = 0;
                    let changes = cookies::diff(&tracked.snapshot, &current, at);
                    tracked.snapshot = current;
                    if changes.len() > 0 {
                        debug!(screen = *number, changed = changes.len(), "cookies changed");
                        state.jar.apply(&changes, at);
                    }
                    read.push(*number);
                }
                Err(error) => {
                    let tracked = state.browsers.get_mut(number).expect("tracked above");
                    tracked.failures += 1;
                    debug!(screen = *number, %error, failures = tracked.failures, "cookie read failed");
                    if tracked.failures >= MAX_FAILURES {
                        warn!(
                            screen = *number,
                            "browser stopped answering, no longer sharing its cookies"
                        );
                        state.browsers.remove(number);
                    }
                }
            }
        }
        for number in read {
            let at = now();
            let Some(tracked) = state.browsers.get(&number) else {
                continue;
            };
            let (port, pending) = (tracked.port, state.jar.pending(&tracked.snapshot, at));
            if pending.len() == 0 {
                continue;
            }
            let written = write_cookies(port, &pending).await;
            let reread = read_cookies(port).await;
            match (written, reread) {
                (Ok(count), Ok(current)) => {
                    debug!(screen = number, pushed = count, "cookies pushed");
                    if let Some(tracked) = state.browsers.get_mut(&number) {
                        tracked.snapshot = current;
                    }
                }
                (Err(error), _) | (_, Err(error)) => {
                    debug!(screen = number, %error, "cookie push failed");
                }
            }
        }
        state.save(false).await;
    }
}

async fn load_jar_into(jar: &Jar, port: u16) -> Result<(Snapshot, usize)> {
    let live = jar.live(now());
    let loaded = live.len();
    let mut connection = Connection::browser(port).await?;
    if !live.is_empty() {
        connection
            .set_cookies(live.iter().map(cookies::to_param).collect())
            .await?;
    }
    let snapshot = cookies::snapshot(connection.get_cookies().await?);
    Ok((snapshot, loaded))
}

impl State {
    async fn save(&mut self, force: bool) {
        if !self.jar.is_dirty() || (!force && self.last_save.elapsed() < SAVE_INTERVAL) {
            return;
        }
        self.jar.prune(now());
        let (path, text) = (self.path.clone(), self.jar.to_json());
        match tokio::task::spawn_blocking(move || write_atomic(&path, &text)).await {
            Ok(Ok(())) => {
                self.jar.mark_saved();
                self.last_save = Instant::now();
            }
            Ok(Err(error)) => warn!(%error, "could not save the cookie jar"),
            Err(error) => warn!(%error, "saving the cookie jar failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_saved_jar_is_private_and_replaces_the_old_file_whole() {
        let dir = std::env::temp_dir().join(format!("computerd-jar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub/cookies.json");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_or_broken_jar_loads_as_empty() {
        let dir = std::env::temp_dir().join(format!("computerd-jar-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            load(&dir.join("missing.json")).live(0.0),
            Vec::<serde_json::Value>::new()
        );
        std::fs::write(dir.join("bad.json"), "{").unwrap();
        assert_eq!(
            load(&dir.join("bad.json")).live(0.0),
            Vec::<serde_json::Value>::new()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
