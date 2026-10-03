use std::time::Duration;

use anyhow::{Context, Result, bail};
use computer_protocol::{
    ApiError, CreateSession, Health, Observation, PROTOCOL_VERSION, ScreenSize, SessionCreated,
    SessionId, SessionTitle, VERSION,
};
use reqwest::StatusCode;

use crate::computer::Endpoint;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(40);
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_DEADLINE: Duration = Duration::from_secs(60);
const HEALTH_RETRY: Duration = Duration::from_millis(250);

/// Why a call to a session failed in a way the agent can fix by starting over.
#[derive(Debug, thiserror::Error)]
#[error("unknown session, call start_computer first")]
pub struct UnknownSession;

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(endpoint: &Endpoint) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            http,
            base: format!("http://127.0.0.1:{}", endpoint.port),
            token: endpoint.token.clone(),
        })
    }

    async fn health(&self) -> Result<Health> {
        self.http
            .get(format!("{}/health", self.base))
            .bearer_auth(&self.token)
            .timeout(HEALTH_REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .map_err(Into::into)
    }

    /// Waits until `computerd` answers with the protocol version this build speaks.
    pub async fn wait_until_ready(&self, container: &str) -> Result<()> {
        let started = tokio::time::Instant::now();
        let last_error = loop {
            match self.health().await {
                Ok(health) if health.protocol_version == PROTOCOL_VERSION => return Ok(()),
                Ok(health) => bail!(
                    "the computer speaks protocol {} (version {}) but this server speaks {PROTOCOL_VERSION} (version {VERSION}), stop the computer with `docker stop {container}` so it can upgrade",
                    health.protocol_version,
                    health.version,
                ),
                Err(error) if started.elapsed() >= HEALTH_DEADLINE => break error,
                Err(_) => tokio::time::sleep(HEALTH_RETRY).await,
            }
        };
        Err(last_error).with_context(|| {
            format!(
                "the computer did not become ready within {} s, check `docker logs {container}`",
                HEALTH_DEADLINE.as_secs()
            )
        })
    }

    pub async fn create_session(
        &self,
        title: SessionTitle,
        screen_size: ScreenSize,
    ) -> Result<SessionId> {
        let created: SessionCreated = self
            .http
            .post(format!("{}/sessions", self.base))
            .bearer_auth(&self.token)
            .json(&CreateSession { title, screen_size })
            .send()
            .await
            .context("creating the session")?
            .error_for_status()
            .context("creating the session")?
            .json()
            .await
            .context("reading the new session")?;
        Ok(created.session)
    }

    /// Ends a session. Fails with [`UnknownSession`] when `computerd` does not know it.
    pub async fn end_session(&self, session: &SessionId) -> Result<()> {
        let response = self
            .http
            .delete(format!("{}/sessions/{session}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("ending the session")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(UnknownSession.into());
        }
        response
            .error_for_status()
            .context("ending the session")
            .map(|_| ())
    }
}

impl Client {
    /// Takes a screenshot of the session's screen, opening the screen on the first call.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn observe(&self, session: &SessionId) -> Result<Observation> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/observe", self.base))
            .bearer_auth(&self.token)
            .timeout(OBSERVE_TIMEOUT)
            .send()
            .await
            .context("asking the computer for a screenshot")?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(UnknownSession.into());
        }
        if !status.is_success() {
            let message = match response.json::<ApiError>().await {
                Ok(error) => error.message,
                Err(_) => format!("the computer answered {status}"),
            };
            bail!("{message}");
        }
        response
            .json()
            .await
            .context("reading the screenshot reply")
    }
}
