use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use computer_protocol::{ActReply, ActRequest, Observation, ScreenSize, SessionId, SessionTitle};
use tracing::{info, warn};

use crate::screen::{Numbers, Screen, ScreenError};

const OBSERVE_TIMEOUT: Duration = Duration::from_secs(25);

/// Why a call on a session failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("unknown session")]
    Unknown,
    #[error("all 16 screens are in use, wait for another agent to call end_session and try again")]
    NoFreeScreen,
    /// The request cannot be run as given. The message tells the agent what to change.
    #[error("{0}")]
    Rejected(String),
    #[error("{0:#}")]
    Failed(anyhow::Error),
}

struct Session {
    title: SessionTitle,
    screen_size: ScreenSize,
    /// Holds the session's screen. Desktop actions lock it, so they run one at a time.
    screen: Arc<tokio::sync::Mutex<Slot>>,
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
#[derive(Clone, Default)]
pub struct Sessions {
    map: Arc<Mutex<HashMap<SessionId, Arc<Session>>>>,
    numbers: Numbers,
}

impl Sessions {
    pub fn insert(&self, id: SessionId, title: SessionTitle, screen_size: ScreenSize) {
        info!(session = %id, title = title.as_str(), %screen_size, "session started");
        let session = Session {
            title,
            screen_size,
            screen: Arc::new(tokio::sync::Mutex::new(Slot::Unopened)),
        };
        self.lock().insert(id, Arc::new(session));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Arc<Session>>> {
        self.map
            .lock()
            .expect("the session lock is only held for short map updates")
    }

    fn get(&self, id: &SessionId) -> Result<Arc<Session>, SessionError> {
        self.lock().get(id).cloned().ok_or(SessionError::Unknown)
    }

    /// Ends a session and closes its screen.
    pub async fn end(&self, id: &SessionId) -> Result<(), SessionError> {
        let session = self.lock().remove(id).ok_or(SessionError::Unknown)?;
        info!(session = %id, title = session.title.as_str(), "session ended");
        session.screen.lock().await.close().await;
        Ok(())
    }

    /// Captures the session's screen, opening it first when this is the session's first call.
    ///
    /// A screen that died or hung is closed and reopened by the next call.
    pub async fn observe(&self, id: &SessionId) -> Result<Observation, SessionError> {
        self.with_screen(id, OBSERVE_TIMEOUT, async |screen| {
            Ok(screen.observe().await?)
        })
        .await
    }

    /// Runs a batch of actions on the session's screen, opening it first when needed.
    ///
    /// Batches of one session run one at a time, in the order they arrive.
    pub async fn act(&self, id: &SessionId, request: ActRequest) -> Result<ActReply, SessionError> {
        let budget = request.time_budget();
        self.with_screen(id, budget, async |screen| screen.act(request).await)
            .await
    }

    /// Runs `call` on the session's screen while holding the session's screen lock.
    ///
    /// A screen that fails the call and is found dead, or that exceeds `timeout`, is closed.
    async fn with_screen<T>(
        &self,
        id: &SessionId,
        timeout: Duration,
        call: impl AsyncFnOnce(&mut Screen) -> Result<T, ScreenError>,
    ) -> Result<T, SessionError> {
        let session = self.get(id)?;
        let mut slot = session.screen.lock().await;
        if matches!(*slot, Slot::Closed) {
            return Err(SessionError::Unknown);
        }
        if matches!(*slot, Slot::Unopened) {
            let lease = self.numbers.lease().ok_or(SessionError::NoFreeScreen)?;
            let screen = Screen::open(lease, session.screen_size)
                .await
                .map_err(SessionError::Failed)?;
            info!(session = %id, screen = screen.number(), "screen assigned");
            *slot = Slot::Open(Box::new(screen));
        }
        let Slot::Open(screen) = &mut *slot else {
            unreachable!("the slot was opened above");
        };
        let failure = match tokio::time::timeout(timeout, call(screen)).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(ScreenError::Rejected(message))) => return Err(SessionError::Rejected(message)),
            Ok(Err(ScreenError::Failed(error))) => match screen.check_alive() {
                Ok(()) => return Err(SessionError::Failed(error)),
                Err(dead) => dead,
            },
            Err(_) => anyhow::anyhow!("the call timed out after {} s", timeout.as_secs()),
        };
        warn!(session = %id, error = %format!("{failure:#}"), "closing a broken screen");
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

    /// Closes every screen. Called when `computerd` shuts down.
    pub async fn close_all(&self) {
        let sessions: Vec<Arc<Session>> = self.lock().drain().map(|(_, session)| session).collect();
        for session in sessions {
            if let Ok(mut slot) =
                tokio::time::timeout(Duration::from_secs(10), session.screen.lock()).await
            {
                slot.close().await;
            } else {
                warn!(
                    title = session.title.as_str(),
                    "screen was busy at shutdown"
                );
            }
        }
    }
}
