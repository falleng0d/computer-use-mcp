//! Keeps the cookies of every running browser in step through one jar saved in home.
//!
//! `DevTools` has no cookie-change event, so every pass reads each browser's cookies, folds what
//! changed into the jar, and writes the jar's differences into the other browsers. The state lock
//! is only held to copy values in and out, never across a call to a browser, so a browser that
//! hangs cannot hold up the others.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    cookies::{self, Jar, Pending, Snapshot},
    devtools::Connection,
};

const JAR_FILE: &str = ".local/share/computer-use/cookies.json";
const SYNC_INTERVAL: Duration = Duration::from_secs(2);
const SAVE_INTERVAL: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(6);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const PASS_TIMEOUT: Duration = Duration::from_secs(20);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);
const FINAL_SYNC_TIMEOUT: Duration = Duration::from_secs(4);
/// Reads in a row that may fail before a browser is no longer tracked.
const MAX_FAILURES: u32 = 30;

static SHARED: OnceLock<CookieSync> = OnceLock::new();

/// The jar and the browsers that share it.
pub struct CookieSync {
    state: Mutex<State>,
    /// Held while the jar is written, so two saves never share the temporary file.
    saving: tokio::sync::Mutex<()>,
    epochs: AtomicU64,
}

struct State {
    jar: Jar,
    path: PathBuf,
    browsers: BTreeMap<u8, Tracked>,
    last_save: Instant,
}

/// A browser that shares cookies. A new `epoch` marks a new Chromium on the same screen number,
/// so a slow answer from the old one is not applied to the new one.
struct Tracked {
    epoch: u64,
    port: u16,
    snapshot: Snapshot,
    failures: u32,
}

struct Push {
    number: u8,
    epoch: u64,
    port: u16,
    pending: Pending,
    before: Snapshot,
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
    let mut jar = loaded.unwrap_or_else(|_| {
        warn!("could not read the cookie jar, starting empty");
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
        saving: tokio::sync::Mutex::new(()),
        epochs: AtomicU64::new(0),
    })
}

/// The installed sync, when `computerd` runs with one.
pub fn shared() -> Option<&'static CookieSync> {
    SHARED.get()
}

/// Reads the saved jar. A jar that cannot be read is kept next to it as `.bad` and replaced.
fn load(path: &Path) -> Jar {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(kind = ?error.kind(), "could not read the cookie jar, starting empty");
            }
            return Jar::default();
        }
    };
    Jar::from_json(&text).unwrap_or_else(|_| {
        let kept = path.with_extension("json.bad");
        warn!(
            kept = %kept.display(),
            "the cookie jar is unreadable or from another version, keeping it and starting empty"
        );
        let _ = std::fs::rename(path, kept);
        Jar::default()
    })
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
    let cookies = tokio::time::timeout(READ_TIMEOUT, read)
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
    tokio::time::timeout(WRITE_TIMEOUT, write)
        .await
        .context("writing cookies timed out")?
}

async fn load_jar_into(live: Vec<Value>, port: u16) -> Result<Snapshot> {
    let mut connection = Connection::browser(port).await?;
    if !live.is_empty() {
        connection
            .set_cookies(live.iter().map(cookies::to_param).collect())
            .await?;
    }
    Ok(cookies::snapshot(connection.get_cookies().await?))
}

impl CookieSync {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("sync state is only held for short copies")
    }

    /// Starts sharing with the browser of screen `number`, giving it every cookie in the jar.
    ///
    /// Never fails. The browser still works when `DevTools` does not answer, it just shares nothing.
    pub async fn attach(&self, number: u8, port: u16) {
        let live = self.lock().jar.live(now());
        let loaded = live.len();
        let result = tokio::time::timeout(ATTACH_TIMEOUT, load_jar_into(live, port)).await;
        let snapshot = match result {
            Ok(Ok(snapshot)) => {
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
        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed);
        self.lock().browsers.insert(
            number,
            Tracked {
                epoch,
                port,
                snapshot,
                failures: 0,
            },
        );
    }

    /// Stops sharing with the browser of screen `number` after one last read of its cookies.
    pub async fn detach(&self, number: u8) {
        let Some(tracked) = self.lock().browsers.remove(&number) else {
            return;
        };
        let read = tokio::time::timeout(FINAL_SYNC_TIMEOUT, read_cookies(tracked.port)).await;
        match read {
            Ok(Ok(current)) => {
                let at = now();
                let changes = cookies::diff(&tracked.snapshot, &current, at);
                self.lock().jar.apply(&changes, at);
                debug!(
                    screen = number,
                    changed = changes.len(),
                    "final cookie read"
                );
            }
            Ok(Err(error)) => warn!(screen = number, %error, "final cookie read failed"),
            Err(_) => warn!(screen = number, "final cookie read timed out"),
        }
        self.save(true).await;
    }

    /// Runs a pass every [`SYNC_INTERVAL`] until `stop` is cancelled, then saves the jar.
    pub async fn run(&self, stop: CancellationToken) {
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                () = tokio::time::sleep(SYNC_INTERVAL) => self.pass().await,
            }
        }
        self.save(true).await;
    }

    async fn pass(&self) {
        let targets: Vec<(u8, u64, u16)> = self
            .lock()
            .browsers
            .iter()
            .map(|(number, tracked)| (*number, tracked.epoch, tracked.port))
            .collect();
        let reads = join_all(targets.iter().map(|&(number, epoch, port)| async move {
            (number, epoch, read_cookies(port).await)
        }));
        let Ok(reads) = tokio::time::timeout(PASS_TIMEOUT, reads).await else {
            debug!("cookie reads timed out");
            return;
        };
        let pushes = self.fold_reads(reads);
        let writes = join_all(pushes.into_iter().map(|push| async move {
            let written = write_cookies(push.port, &push.pending).await;
            let reread = read_cookies(push.port).await;
            (push, written.and(reread))
        }));
        if let Ok(done) = tokio::time::timeout(PASS_TIMEOUT, writes).await {
            self.fold_pushes(done);
        } else {
            debug!("cookie writes timed out");
        }
        self.save(false).await;
    }

    /// Folds what the browsers changed into the jar and says what each browser still needs.
    fn fold_reads(&self, reads: Vec<(u8, u64, Result<Snapshot>)>) -> Vec<Push> {
        let mut guard = self.lock();
        let State { jar, browsers, .. } = &mut *guard;
        let at = now();
        let mut readable = Vec::new();
        for (number, epoch, result) in reads {
            let Some(tracked) = browsers
                .get_mut(&number)
                .filter(|tracked| tracked.epoch == epoch)
            else {
                continue;
            };
            match result {
                Ok(current) => {
                    tracked.failures = 0;
                    let changes = cookies::diff(&tracked.snapshot, &current, at);
                    tracked.snapshot = current;
                    if changes.len() > 0 {
                        debug!(screen = number, changed = changes.len(), "cookies changed");
                        jar.apply(&changes, at);
                    }
                    readable.push(number);
                }
                Err(error) => {
                    tracked.failures += 1;
                    debug!(screen = number, %error, failures = tracked.failures, "cookie read failed");
                    if tracked.failures >= MAX_FAILURES {
                        warn!(
                            screen = number,
                            "browser stopped answering, no longer sharing its cookies"
                        );
                        browsers.remove(&number);
                    }
                }
            }
        }
        readable
            .into_iter()
            .filter_map(|number| {
                let tracked = browsers.get(&number)?;
                let pending = jar.pending(&tracked.snapshot, at);
                (pending.len() > 0).then(|| Push {
                    number,
                    epoch: tracked.epoch,
                    port: tracked.port,
                    pending,
                    before: tracked.snapshot.clone(),
                })
            })
            .collect()
    }

    /// Records what the browsers hold after a push. Changes a page made meanwhile go to the jar.
    fn fold_pushes(&self, done: Vec<(Push, Result<Snapshot>)>) {
        let mut guard = self.lock();
        let State { jar, browsers, .. } = &mut *guard;
        let at = now();
        for (push, result) in done {
            match result {
                Ok(reread) => {
                    let Some(tracked) = browsers
                        .get_mut(&push.number)
                        .filter(|tracked| tracked.epoch == push.epoch)
                    else {
                        continue;
                    };
                    let changes =
                        cookies::changes_after_push(&push.before, &reread, &push.pending, at);
                    debug!(
                        screen = push.number,
                        pushed = push.pending.len(),
                        changed = changes.len(),
                        "cookies pushed"
                    );
                    jar.apply(&changes, at);
                    tracked.snapshot = reread;
                }
                Err(error) => {
                    debug!(screen = push.number, %error, "cookie push failed");
                }
            }
        }
    }

    /// Writes the jar when it changed. Without `force` it writes at most every [`SAVE_INTERVAL`].
    async fn save(&self, force: bool) {
        let _writing = self.saving.lock().await;
        let (path, text) = {
            let mut state = self.lock();
            if !force && state.last_save.elapsed() < SAVE_INTERVAL {
                return;
            }
            let Some(text) = state.jar.take_unsaved(now()) else {
                return;
            };
            state.last_save = Instant::now();
            (state.path.clone(), text)
        };
        let written = tokio::task::spawn_blocking(move || write_atomic(&path, &text)).await;
        if !matches!(written, Ok(Ok(()))) {
            warn!("could not save the cookie jar");
            self.lock().jar.mark_unsaved();
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
    fn a_missing_jar_loads_empty_and_a_broken_one_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("computerd-jar-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            load(&dir.join("missing.json")).live(0.0),
            Vec::<serde_json::Value>::new()
        );
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{").unwrap();
        assert_eq!(load(&bad).live(0.0), Vec::<serde_json::Value>::new());
        assert!(!bad.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("bad.json.bad")).unwrap(),
            "{"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
