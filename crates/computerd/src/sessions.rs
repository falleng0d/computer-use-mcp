use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use computer_protocol::{Observation, ScreenSize, SessionId, SessionTitle};
use tracing::{info, warn};

use crate::screen::{Numbers, Screen};

const OBSERVE_TIMEOUT: Duration = Duration::from_secs(25);

/// Why a call on a session failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("unknown session")]
    Unknown,
    #[error("all 16 screens are in use, wait for another agent to call end_session and try again")]
    NoFreeScreen,
    #[error("{0:#}")]
    Failed(anyhow::Error),
}

struct Session {
    title: SessionTitle,
    screen_size: ScreenSize,
    /// Holds the session's screen. Desktop actions lock it, so they run one at a time.
    screen: Arc<tokio::sync::Mutex<Option<Screen>>>,
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
            screen: Arc::default(),
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
        let screen = session.screen.lock().await.take();
        if let Some(screen) = screen {
            screen.close().await;
        }
        Ok(())
    }

    /// Captures the session's screen, opening it first when this is the session's first call.
    pub async fn observe(&self, id: &SessionId) -> Result<Observation, SessionError> {
        let session = self.get(id)?;
        let work = async {
            let mut slot = session.screen.lock().await;
            if slot.is_none() {
                let lease = self.numbers.lease().ok_or(SessionError::NoFreeScreen)?;
                let screen = Screen::open(lease, session.screen_size)
                    .await
                    .map_err(SessionError::Failed)?;
                info!(session = %id, screen = screen.number(), "screen assigned");
                *slot = Some(screen);
            }
            let screen = slot.as_mut().expect("the screen was opened above");
            screen.observe().await.map_err(SessionError::Failed)
        };
        match tokio::time::timeout(OBSERVE_TIMEOUT, work).await {
            Ok(result) => result,
            Err(_) => Err(SessionError::Failed(anyhow::anyhow!(
                "taking the screenshot timed out after {} s",
                OBSERVE_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Closes every screen. Called when `computerd` shuts down.
    pub async fn close_all(&self) {
        let sessions: Vec<Arc<Session>> = self.lock().drain().map(|(_, session)| session).collect();
        for session in sessions {
            if let Ok(mut slot) =
                tokio::time::timeout(Duration::from_secs(10), session.screen.lock()).await
            {
                if let Some(screen) = slot.take() {
                    screen.close().await;
                }
            } else {
                warn!(
                    title = session.title.as_str(),
                    "screen was busy at shutdown"
                );
            }
        }
    }
}
