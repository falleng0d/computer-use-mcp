use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use computer_protocol::{
    ActReply, ActRequest, CreateSession, DEFAULT_PORT_BASE, LaunchAppRequest, ListFilesReply,
    ListFilesRequest, Observation, OpenPathRequest, OwnerId, ReadFileReply, ReadFileRequest,
    ScreenSize, SessionId, SessionTitle, SetCwdReply, SetCwdRequest, ShellReply, ShellRequest,
    ShellTimeouts, WriteFileReply, WriteFileRequest,
};
use serde::Serialize;
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{info, warn};

use crate::{
    exec, files,
    hub::Hub,
    liveness::{self, EndReason, Liveness},
    numbers::Numbers,
    screen::{Screen, ScreenError},
    workdir,
};

const OBSERVE_TIMEOUT: Duration = Duration::from_secs(25);
/// Longest an open or launch call may take, which covers starting the browser.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(75);
const REAP_INTERVAL: Duration = Duration::from_secs(1);
/// How many sessions that ended on their own are remembered to tell agents why.
const ENDED_MEMORY: usize = 256;
/// Longest wait for a busy screen when the computer shuts down.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .expect("session fields are only held for short copies")
}

/// Why a call on a session failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("unknown session")]
    Unknown,
    #[error("this session ended because {0}")]
    Ended(EndReason),
    #[error("all 16 screens are in use, wait for another agent to call end_session and try again")]
    NoFreeScreen,
    /// The request cannot be run as given. The message tells the agent what to change.
    #[error("{0}")]
    Rejected(String),
    #[error("{0:#}")]
    Failed(anyhow::Error),
}

/// One session as the viewer page lists it.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SessionView {
    /// First 8 characters of the session id, enough to tell sessions apart on the page.
    pub id: String,
    pub title: String,
    pub screen: Option<u8>,
    /// Start time in seconds since the Unix epoch.
    pub started: u64,
    /// Viewers attached to the session's screen.
    pub viewers: usize,
}

struct Session {
    title: SessionTitle,
    owner: OwnerId,
    idle: Duration,
    created: Instant,
    started: u64,
    /// Time the last agent call started or finished.
    last_activity: Mutex<Instant>,
    /// Agent calls running now.
    calls: AtomicUsize,
    screen_size: ScreenSize,
    shell_timeouts: ShellTimeouts,
    /// Cancelled when the session ends or the computer shuts down. Kills the session's running commands.
    cancel: CancellationToken,
    /// Folder shell commands start in.
    cwd: Mutex<PathBuf>,
    /// Display number of the open screen. Shell calls read it without touching the screen lock.
    display: Mutex<Option<u8>>,
    /// Screen opened by a call that has not yet returned successfully. The next successful reply reports it once.
    announce: Mutex<Option<u8>>,
    /// Holds the session's screen. Desktop actions lock it, so they run one at a time.
    screen: Arc<tokio::sync::Mutex<Slot>>,
}

/// Marks a session busy while an agent call runs and counts the call's start and end as activity.
struct Call {
    session: Arc<Session>,
}

impl Call {
    fn start(session: Arc<Session>) -> Self {
        session.calls.fetch_add(1, Ordering::SeqCst);
        *lock(&session.last_activity) = Instant::now();
        Self { session }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        *lock(&self.session.last_activity) = Instant::now();
        self.session.calls.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Session {
    fn liveness(&self, now: Instant, owner_seen: Option<Instant>, viewers: usize) -> Liveness {
        Liveness {
            now,
            created: self.created,
            last_activity: *lock(&self.last_activity),
            owner_seen,
            idle: self.idle,
            call_running: self.calls.load(Ordering::SeqCst) > 0,
            viewers,
        }
    }

    fn screen(&self) -> Option<u8> {
        *lock(&self.display)
    }
}

/// Closes the screen of a session that has ended once no viewer is attached to it,
/// waiting a bounded time for a call that still holds it.
async fn close_screen(session: Arc<Session>, hub: Hub, shutdown: CancellationToken) {
    if let Some(screen) = session.screen() {
        tokio::select! {
            () = hub.detached(screen) => {}
            () = shutdown.cancelled() => {}
        }
    }
    if let Ok(mut slot) = tokio::time::timeout(CLOSE_TIMEOUT, session.screen.lock()).await {
        slot.close().await;
    } else {
        warn!(
            title = session.title.as_str(),
            "screen was busy, its processes are killed when the last call on it ends"
        );
    }
}

/// State of a session's screen. `Closed` is final, so a call racing with `end` cannot open a screen.
enum Slot {
    Unopened,
    Open(Box<Screen>),
    Closed,
}

impl Slot {
    /// Marks the slot closed and stops the screen it held.
    async fn close(&mut self) {
        if let Self::Open(screen) = std::mem::replace(self, Self::Closed) {
            (*screen).close().await;
        }
    }
}

/// Every session of the computer and the screens they own.
#[derive(Clone)]
pub struct Sessions {
    map: Arc<Mutex<HashMap<SessionId, Arc<Session>>>>,
    /// Time of each owner's last heartbeat. Only owners with sessions are kept.
    owners: Arc<Mutex<HashMap<OwnerId, Instant>>>,
    /// Sessions that ended without the agent asking, and why, oldest first.
    ended: Arc<Mutex<VecDeque<(SessionId, EndReason)>>>,
    /// Screen closes of ended sessions, awaited at shutdown.
    closing: TaskTracker,
    hub: Hub,
    numbers: Numbers,
    home: PathBuf,
    shutdown: CancellationToken,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new(Hub::new(DEFAULT_PORT_BASE))
    }
}

impl Sessions {
    pub fn new(hub: Hub) -> Self {
        Self {
            map: Arc::default(),
            owners: Arc::default(),
            ended: Arc::default(),
            closing: TaskTracker::new(),
            hub,
            numbers: Numbers::default(),
            home: workdir::home_dir(),
            shutdown: CancellationToken::new(),
        }
    }

    pub fn hub(&self) -> Hub {
        self.hub.clone()
    }

    /// The sessions the viewer page lists, oldest first.
    pub fn views(&self) -> Vec<SessionView> {
        let mut views: Vec<_> = self
            .lock()
            .iter()
            .map(|(id, session)| {
                let screen = session.screen();
                SessionView {
                    id: id.to_string().chars().take(8).collect(),
                    title: session.title.as_str().to_owned(),
                    screen,
                    started: session.started,
                    viewers: screen.map_or(0, |screen| self.hub.viewers(screen)),
                }
            })
            .collect();
        views.sort_by(|a, b| (a.started, &a.id).cmp(&(b.started, &b.id)));
        views
    }

    pub fn insert(&self, id: SessionId, request: CreateSession) {
        let idle = Duration::from_secs(u64::from(request.idle_secs.get()));
        info!(
            session = %id,
            title = request.title.as_str(),
            screen_size = %request.screen_size,
            owner = %request.owner,
            idle_secs = idle.as_secs(),
            "session started"
        );
        let now = Instant::now();
        let session = Session {
            title: request.title,
            owner: request.owner,
            idle,
            created: now,
            started: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            last_activity: Mutex::new(now),
            calls: AtomicUsize::new(0),
            screen_size: request.screen_size,
            shell_timeouts: request.shell_timeouts,
            cancel: self.shutdown.child_token(),
            cwd: Mutex::new(self.home.clone()),
            display: Mutex::new(None),
            announce: Mutex::new(None),
            screen: Arc::new(tokio::sync::Mutex::new(Slot::Unopened)),
        };
        self.lock().insert(id, Arc::new(session));
        self.hub.notify();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Arc<Session>>> {
        self.map
            .lock()
            .expect("the session lock is only held for short map updates")
    }

    fn get(&self, id: &SessionId) -> Result<Arc<Session>, SessionError> {
        self.lock().get(id).cloned().ok_or_else(|| self.missing(id))
    }

    /// The error for a session that is not in the map, with the reason when it ended recently.
    fn missing(&self, id: &SessionId) -> SessionError {
        lock(&self.ended)
            .iter()
            .find(|(ended_id, _)| ended_id == id)
            .map_or(SessionError::Unknown, |(_, reason)| {
                SessionError::Ended(*reason)
            })
    }

    /// Looks up a session for an agent call. The call counts as activity until the guard drops.
    fn begin(&self, id: &SessionId) -> Result<Call, SessionError> {
        let session = self.get(id)?;
        if session.cancel.is_cancelled() {
            return Err(self.missing(id));
        }
        Ok(Call::start(session))
    }

    /// Records a heartbeat for every session of `owner`.
    pub fn heartbeat(&self, owner: &OwnerId) {
        let has_sessions = self.lock().values().any(|s| s.owner == *owner);
        if has_sessions {
            lock(&self.owners).insert(owner.clone(), Instant::now());
        }
    }

    /// Ends a session on the agent's request.
    pub async fn end(&self, id: &SessionId) -> Result<(), SessionError> {
        let removed = self.lock().remove(id);
        let session = removed.ok_or_else(|| self.missing(id))?;
        if let Some(closed) = self.finish(id, session, EndReason::Agent) {
            let _ = closed.await;
        }
        Ok(())
    }

    /// Ends every session of `owner` and returns how many there were.
    ///
    /// The sessions are ended even when the caller stops waiting for their screens to close.
    pub async fn end_owner(&self, owner: &OwnerId) -> usize {
        let ending: Vec<_> = {
            let mut map = self.lock();
            let ids: Vec<_> = map
                .iter()
                .filter(|(_, session)| session.owner == *owner)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|session| (id, session)))
                .collect()
        };
        let closing: Vec<_> = ending
            .into_iter()
            .map(|(id, session)| self.finish(&id, session, EndReason::OwnerLeft))
            .collect();
        let count = closing.len();
        for closed in closing.into_iter().flatten() {
            let _ = closed.await;
        }
        count
    }

    /// Ends the sessions that are due now. Screens close in the background.
    fn reap(&self) {
        let now = Instant::now();
        let due: Vec<_> = {
            let mut map = self.lock();
            let owners = lock(&self.owners);
            let due: Vec<_> = map
                .iter()
                .filter_map(|(id, session)| {
                    let seen = owners.get(&session.owner).copied();
                    let viewers = session
                        .screen()
                        .map_or(0, |screen| self.hub.viewers(screen));
                    if viewers > 0 {
                        *lock(&session.last_activity) = now;
                    }
                    liveness::end_reason(&session.liveness(now, seen, viewers))
                        .map(|why| (id.clone(), why))
                })
                .collect();
            due.into_iter()
                .filter_map(|(id, why)| map.remove(&id).map(|session| (id, session, why)))
                .collect()
        };
        for (id, session, why) in due {
            drop(self.finish(&id, session, why));
        }
        let live: HashSet<OwnerId> = self.lock().values().map(|s| s.owner.clone()).collect();
        lock(&self.owners).retain(|owner, _| live.contains(owner));
    }

    /// Checks every second which sessions are due to end, until `stop` is cancelled.
    pub async fn reap_until(&self, stop: CancellationToken) {
        let mut tick = tokio::time::interval(REAP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                _ = tick.tick() => self.reap(),
            }
        }
    }

    /// Finishes a session already removed from the map.
    ///
    /// Remembers why and kills its commands and running screen call right away, then closes its
    /// screen in a task `Sessions` owns once the last viewer of the screen disconnects. The returned
    /// handle completes when the screen is closed and dropping it does not stop the close. It is
    /// `None` when a viewer is attached, because the close then waits for the viewer.
    fn finish(
        &self,
        id: &SessionId,
        session: Arc<Session>,
        why: EndReason,
    ) -> Option<JoinHandle<()>> {
        info!(
            session = %id,
            title = session.title.as_str(),
            owner = %session.owner,
            reason = why.label(),
            "session ended"
        );
        if why != EndReason::Agent {
            let mut ended = lock(&self.ended);
            if ended.len() >= ENDED_MEMORY {
                ended.pop_front();
            }
            ended.push_back((id.clone(), why));
        }
        session.cancel.cancel();
        let viewed = session
            .screen()
            .is_some_and(|screen| self.hub.end_screen(screen));
        let closed = self.closing.spawn(close_screen(
            session,
            self.hub.clone(),
            self.shutdown.clone(),
        ));
        self.hub.notify();
        (!viewed).then_some(closed)
    }

    /// Captures the session's screen, opening it first when this is the session's first call.
    ///
    /// A screen that died or hung is closed and reopened by the next call.
    pub async fn observe(&self, id: &SessionId) -> Result<Observation, SessionError> {
        let (mut observation, opened_screen) = self
            .with_screen(id, OBSERVE_TIMEOUT, async |screen| {
                Ok(screen.observe().await?)
            })
            .await?;
        observation.opened_screen = opened_screen;
        Ok(observation)
    }

    /// Runs a batch of actions on the session's screen, opening it first when needed.
    ///
    /// Batches of one session run one at a time, in the order they arrive.
    pub async fn act(&self, id: &SessionId, request: ActRequest) -> Result<ActReply, SessionError> {
        let budget = request.time_budget();
        let cwd = self.cwd_of(id)?;
        let (mut reply, opened_screen) = self
            .with_screen(id, budget, async |screen| screen.act(request, &cwd).await)
            .await?;
        reply.opened_screen = opened_screen;
        Ok(reply)
    }

    fn cwd_of(&self, id: &SessionId) -> Result<PathBuf, SessionError> {
        Ok(lock(&self.get(id)?.cwd).clone())
    }

    /// Opens a file or an http(s) URL on the session's screen, opening the screen first when needed.
    pub async fn open_path(
        &self,
        id: &SessionId,
        request: OpenPathRequest,
    ) -> Result<Observation, SessionError> {
        let cwd = self.cwd_of(id)?;
        let (mut observation, opened_screen) = self
            .with_screen(id, LAUNCH_TIMEOUT, async |screen| {
                screen.open_path(&request.path, &cwd).await
            })
            .await?;
        observation.opened_screen = opened_screen;
        Ok(observation)
    }

    /// Starts or raises an application on the session's screen, opening the screen first when needed.
    pub async fn launch_app(
        &self,
        id: &SessionId,
        request: LaunchAppRequest,
    ) -> Result<Observation, SessionError> {
        let cwd = self.cwd_of(id)?;
        let (mut observation, opened_screen) = self
            .with_screen(id, LAUNCH_TIMEOUT, async |screen| {
                screen
                    .launch_app(&request.application, request.uri.as_deref(), &cwd)
                    .await
            })
            .await?;
        observation.opened_screen = opened_screen;
        Ok(observation)
    }

    /// Shows the browser of an open screen, starting it when it is not running.
    pub async fn show_browser(&self, screen: u8, url: Option<String>) -> Result<(), SessionError> {
        let id = self
            .lock()
            .iter()
            .find(|(_, session)| session.screen() == Some(screen))
            .map(|(id, _)| id.clone())
            .ok_or_else(|| SessionError::Rejected(format!("screen {screen} is not in use")))?;
        let cwd = self.cwd_of(&id)?;
        self.with_screen(&id, LAUNCH_TIMEOUT, async |screen| {
            screen.show_in_browser(url.as_deref(), &cwd).await
        })
        .await
        .map(|_| ())
    }

    /// Runs `call` on the session's screen while holding the session's screen lock.
    /// Also returns the screen number when the screen opened and no successful reply has reported it yet.
    ///
    /// A screen that fails the call and is found dead, or that exceeds `timeout`, is closed.
    async fn with_screen<T>(
        &self,
        id: &SessionId,
        timeout: Duration,
        call: impl AsyncFnOnce(&mut Screen) -> Result<T, ScreenError>,
    ) -> Result<(T, Option<u8>), SessionError> {
        let active = self.begin(id)?;
        let session = &active.session;
        let mut slot = session.screen.lock().await;
        if matches!(*slot, Slot::Closed) || session.cancel.is_cancelled() {
            return Err(self.missing(id));
        }
        if matches!(*slot, Slot::Unopened) {
            let lease = self.numbers.lease().ok_or(SessionError::NoFreeScreen)?;
            let screen = Screen::open(lease, session.screen_size)
                .await
                .map_err(SessionError::Failed)?;
            info!(session = %id, screen = screen.number(), "screen assigned");
            *lock(&session.display) = Some(screen.number());
            info!(
                screen = screen.number(),
                title = session.title.as_str(),
                vnc_port = self.hub.host_vnc_port(screen.number()),
                "screen opened for viewing"
            );
            self.hub.reopen_screen(screen.number());
            self.hub.notify();
            *lock(&session.announce) = Some(screen.number());
            *slot = Slot::Open(Box::new(screen));
        }
        let Slot::Open(screen) = &mut *slot else {
            unreachable!("the slot was opened above");
        };
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(timeout, call(screen)) => outcome,
            () = session.cancel.cancelled() => return Err(self.missing(id)),
        };
        let failure = match outcome {
            Ok(Ok(value)) => return Ok((value, lock(&session.announce).take())),
            Ok(Err(ScreenError::Rejected(message))) => return Err(SessionError::Rejected(message)),
            Ok(Err(ScreenError::Failed(error))) => match screen.check_alive() {
                Ok(()) => return Err(SessionError::Failed(error)),
                Err(dead) => dead,
            },
            Err(_) => anyhow::anyhow!("the call timed out after {} s", timeout.as_secs()),
        };
        warn!(session = %id, error = %format!("{failure:#}"), "closing a broken screen");
        *lock(&session.display) = None;
        *lock(&session.announce) = None;
        self.hub.notify();
        *slot = match std::mem::replace(&mut *slot, Slot::Unopened) {
            Slot::Open(screen) => {
                (*screen).close().await;
                Slot::Unopened
            }
            other => other,
        };
        Err(SessionError::Failed(failure.context(
            "the screen stopped working and was closed, open windows are lost; call computer_observe again to get a fresh screen",
        )))
    }

    /// Runs a shell command in the session's working folder.
    ///
    /// Never waits for the screen lock, and any number of commands of a session may run at once.
    pub async fn shell(
        &self,
        id: &SessionId,
        request: ShellRequest,
    ) -> Result<ShellReply, SessionError> {
        let active = self.begin(id)?;
        let session = &active.session;
        let job = exec::Job {
            command: request.command,
            cwd: lock(&session.cwd).clone(),
            display: *lock(&session.display),
            timeout: session.shell_timeouts.effective(request.timeout_secs),
            cancel: session.cancel.clone(),
        };
        exec::run(job).await.map_err(SessionError::Failed)
    }

    /// Changes the session's working folder and returns its absolute path.
    pub async fn set_cwd(
        &self,
        id: &SessionId,
        request: SetCwdRequest,
    ) -> Result<SetCwdReply, SessionError> {
        let active = self.begin(id)?;
        let session = &active.session;
        let current = lock(&session.cwd).clone();
        let new = workdir::existing_dir(&current, &self.home, &request.path)
            .await
            .map_err(SessionError::Rejected)?;
        lock(&session.cwd).clone_from(&new);
        Ok(SetCwdReply {
            cwd: new.display().to_string(),
        })
    }

    /// Lists a folder. A relative path starts at the session's working folder.
    pub async fn list_files(
        &self,
        id: &SessionId,
        request: ListFilesRequest,
    ) -> Result<ListFilesReply, SessionError> {
        let active = self.begin(id)?;
        let cwd = lock(&active.session.cwd).clone();
        let home = self.home.clone();
        blocking(move || files::list(&cwd, &home, &request)).await
    }

    /// Reads a text file or an image. A relative path starts at the session's working folder.
    pub async fn read_file(
        &self,
        id: &SessionId,
        request: ReadFileRequest,
    ) -> Result<ReadFileReply, SessionError> {
        let active = self.begin(id)?;
        let cwd = lock(&active.session.cwd).clone();
        let home = self.home.clone();
        blocking(move || files::read(&cwd, &home, &request)).await
    }

    /// Writes a text file. A relative path starts at the session's working folder.
    pub async fn write_file(
        &self,
        id: &SessionId,
        request: WriteFileRequest,
    ) -> Result<WriteFileReply, SessionError> {
        let active = self.begin(id)?;
        let cwd = lock(&active.session.cwd).clone();
        let home = self.home.clone();
        blocking(move || files::write(&cwd, &home, &request)).await
    }

    /// Kills every running command and refuses new ones. Called when `computerd` starts to shut down.
    pub fn cancel_all(&self) {
        self.shutdown.cancel();
    }

    /// Closes every screen and waits for the closes of sessions that ended earlier. Called when `computerd` shuts down.
    pub async fn close_all(&self) {
        self.shutdown.cancel();
        let sessions: Vec<Arc<Session>> = self.lock().drain().map(|(_, session)| session).collect();
        for session in sessions {
            session.cancel.cancel();
            drop(self.closing.spawn(close_screen(
                session,
                self.hub.clone(),
                self.shutdown.clone(),
            )));
        }
        self.closing.close();
        self.closing.wait().await;
    }
}

/// Runs blocking file work off the async threads. A refusal becomes a message for the agent.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, SessionError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(SessionError::Rejected),
        Err(error) => Err(SessionError::Failed(
            anyhow::Error::new(error).context("running the file call"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    fn owner(digit: char) -> OwnerId {
        OwnerId::parse(&digit.to_string().repeat(32)).unwrap()
    }

    fn start(sessions: &Sessions, id: char, owner: &OwnerId, idle_secs: u32) -> SessionId {
        let id = SessionId::parse(&id.to_string().repeat(32)).unwrap();
        sessions.insert(
            id.clone(),
            CreateSession {
                title: SessionTitle::parse("task").unwrap(),
                screen_size: ScreenSize::default(),
                shell_timeouts: ShellTimeouts::default(),
                owner: owner.clone(),
                idle_secs: NonZeroU32::new(idle_secs).unwrap(),
            },
        );
        id
    }

    async fn pass(secs: u64) {
        for _ in 0..secs {
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
    }

    fn ended_by(sessions: &Sessions, id: &SessionId) -> Option<EndReason> {
        match sessions.get(id) {
            Err(SessionError::Ended(reason)) => Some(reason),
            _ => None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_reaper_ends_sessions_of_a_silent_owner_and_keeps_those_of_a_live_one() {
        let sessions = Sessions::default();
        let (live, silent) = (owner('a'), owner('b'));
        let kept = start(&sessions, '1', &live, 3600);
        let dropped = start(&sessions, '2', &silent, 3600);
        let stop = CancellationToken::new();
        let reaper = tokio::spawn({
            let (sessions, stop) = (sessions.clone(), stop.clone());
            async move { sessions.reap_until(stop).await }
        });

        for _ in 0..4 {
            pass(10).await;
            sessions.heartbeat(&live);
            sessions.heartbeat(&silent);
        }
        assert!(sessions.get(&kept).is_ok());
        assert!(sessions.get(&dropped).is_ok());

        for _ in 0..4 {
            pass(10).await;
            sessions.heartbeat(&live);
        }
        assert_eq!(ended_by(&sessions, &dropped), Some(EndReason::OwnerGone));
        assert!(sessions.get(&kept).is_ok());

        pass(31).await;
        assert_eq!(ended_by(&sessions, &kept), Some(EndReason::OwnerGone));
        stop.cancel();
        reaper.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_sessions_end_but_calls_and_running_commands_keep_a_session() {
        let sessions = Sessions::default();
        let who = owner('a');
        let quiet = start(&sessions, '1', &who, 20);
        let called = start(&sessions, '2', &who, 20);
        let running = start(&sessions, '3', &who, 20);
        let stop = CancellationToken::new();
        let reaper = tokio::spawn({
            let (sessions, stop) = (sessions.clone(), stop.clone());
            async move { sessions.reap_until(stop).await }
        });
        let command = sessions.begin(&running).unwrap();

        for step in 1..=4 {
            pass(10).await;
            sessions.heartbeat(&who);
            if step < 4 {
                drop(sessions.begin(&called).unwrap());
            }
        }
        assert_eq!(
            ended_by(&sessions, &quiet),
            Some(EndReason::Idle(Duration::from_secs(20)))
        );
        assert_eq!(ended_by(&sessions, &called), None);
        assert!(sessions.get(&called).is_ok());
        assert!(sessions.get(&running).is_ok());

        drop(command);
        pass(21).await;
        assert_eq!(
            ended_by(&sessions, &running),
            Some(EndReason::Idle(Duration::from_secs(20)))
        );
        stop.cancel();
        reaper.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_screen_does_not_delay_other_sessions_from_ending_on_time() {
        let sessions = Sessions::default();
        let (silent, live) = (owner('a'), owner('b'));
        let stuck = start(&sessions, '1', &silent, 3600);
        let quiet = start(&sessions, '2', &live, 40);
        let stop = CancellationToken::new();
        let reaper = tokio::spawn({
            let (sessions, stop) = (sessions.clone(), stop.clone());
            async move { sessions.reap_until(stop).await }
        });
        let screen = sessions.get(&stuck).unwrap();
        let busy = screen.screen.lock().await;

        for _ in 0..3 {
            pass(10).await;
            sessions.heartbeat(&live);
        }
        pass(1).await;
        assert_eq!(ended_by(&sessions, &stuck), Some(EndReason::OwnerGone));
        assert!(screen.cancel.is_cancelled());
        assert!(sessions.get(&quiet).is_ok());

        pass(9).await;
        assert_eq!(
            ended_by(&sessions, &quiet),
            Some(EndReason::Idle(Duration::from_secs(40)))
        );
        drop(busy);
        stop.cancel();
        reaper.await.unwrap();
        sessions.close_all().await;
    }

    #[tokio::test]
    async fn ending_an_owner_ends_only_its_sessions_and_says_why() {
        let sessions = Sessions::default();
        let (leaving, staying) = (owner('a'), owner('b'));
        let gone = start(&sessions, '1', &leaving, 3600);
        let kept = start(&sessions, '2', &staying, 3600);
        assert_eq!(sessions.end_owner(&leaving).await, 1);
        assert_eq!(ended_by(&sessions, &gone), Some(EndReason::OwnerLeft));
        assert!(sessions.get(&kept).is_ok());
        assert!(matches!(
            sessions.end(&gone).await,
            Err(SessionError::Ended(EndReason::OwnerLeft))
        ));
        sessions.end(&kept).await.unwrap();
        assert!(matches!(
            sessions.end(&kept).await,
            Err(SessionError::Unknown)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_viewed_session_ends_at_once_but_its_screen_closes_when_the_viewer_leaves() {
        let sessions = Sessions::default();
        let who = owner('a');
        let id = start(&sessions, '1', &who, 3600);
        let session = sessions.get(&id).unwrap();
        *lock(&session.display) = Some(3);
        let viewer = sessions.hub().attach(3).unwrap();

        sessions.end(&id).await.unwrap();
        pass(5).await;
        assert!(session.cancel.is_cancelled());
        assert!(sessions.get(&id).is_err());
        assert_eq!(sessions.closing.len(), 1);

        drop(viewer);
        pass(1).await;
        assert_eq!(sessions.closing.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_viewer_pauses_the_idle_timer_and_it_restarts_when_the_viewer_leaves() {
        let sessions = Sessions::default();
        let who = owner('a');
        let id = start(&sessions, '1', &who, 20);
        *lock(&sessions.get(&id).unwrap().display) = Some(2);
        let viewer = sessions.hub().attach(2).unwrap();
        let stop = CancellationToken::new();
        let reaper = tokio::spawn({
            let (sessions, stop) = (sessions.clone(), stop.clone());
            async move { sessions.reap_until(stop).await }
        });

        for _ in 0..6 {
            pass(10).await;
            sessions.heartbeat(&who);
        }
        assert!(sessions.get(&id).is_ok());

        drop(viewer);
        {
            pass(10).await;
            sessions.heartbeat(&who);
        }
        assert!(sessions.get(&id).is_ok());
        pass(11).await;
        sessions.heartbeat(&who);
        assert_eq!(
            ended_by(&sessions, &id),
            Some(EndReason::Idle(Duration::from_secs(20)))
        );
        stop.cancel();
        reaper.await.unwrap();
    }
}
