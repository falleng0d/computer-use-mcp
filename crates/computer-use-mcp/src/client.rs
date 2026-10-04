use std::{num::NonZeroU32, time::Duration};

use anyhow::{Context, Result, bail};
use computer_protocol::{
    ActReply, ActRequest, ApiError, CreateSession, Health, LaunchAppRequest, ListFilesReply,
    ListFilesRequest, Observation, OpenPathRequest, OwnerId, PROTOCOL_VERSION, ReadFileReply,
    ReadFileRequest, ScreenSize, SessionCreated, SessionId, SessionTitle, SetCwdReply,
    SetCwdRequest, ShellReply, ShellRequest, ShellTimeouts, VERSION, ViewerInfo, WriteFileReply,
    WriteFileRequest,
};
use reqwest::StatusCode;

use crate::computer::Endpoint;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(40);
/// Time on top of the batch's own budget for the HTTP round trip and for waiting behind another batch of the session.
const ACT_MARGIN: Duration = Duration::from_secs(120);
/// Time on top of a command's own timeout for killing it and for the HTTP round trip.
const SHELL_MARGIN: Duration = Duration::from_secs(30);
const FILE_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest wait for `computerd` to start an application or a page, which includes starting the browser.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(100);
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_DEADLINE: Duration = Duration::from_secs(60);
/// Longest wait for `computerd` when the MCP server is shutting down.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_RETRY: Duration = Duration::from_millis(250);

/// A call named a session `computerd` does not have, which the agent fixes by starting over.
#[derive(Debug, thiserror::Error)]
#[error("unknown session, call start_computer first")]
pub struct UnknownSession {
    /// Why the session ended, when `computerd` still remembers it.
    pub reason: Option<String>,
}

/// What an agent is told when the running computer speaks another protocol than this server.
///
/// An older computer is upgraded once it is stopped. A newer one is never moved back, so the
/// server has to be updated instead.
fn protocol_mismatch(computer_protocol: u32, computer_version: &str, container: &str) -> String {
    if computer_protocol > PROTOCOL_VERSION {
        format!(
            "This computer runs protocol {computer_protocol} (version {computer_version}), newer than the protocol {PROTOCOL_VERSION} (version {VERSION}) this MCP server speaks. Update the computer-use-mcp binary. The computer is never moved to an older image."
        )
    } else {
        format!(
            "This computer runs protocol {computer_protocol} (version {computer_version}), but this MCP server speaks {PROTOCOL_VERSION} (version {VERSION}). Stop it (`docker stop {container}`) so the next call can upgrade it."
        )
    }
}

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
                Ok(health) => bail!(protocol_mismatch(
                    health.protocol_version,
                    &health.version,
                    container
                )),
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
        shell_timeouts: ShellTimeouts,
        owner: OwnerId,
        idle_secs: NonZeroU32,
    ) -> Result<SessionId> {
        let created: SessionCreated = self
            .http
            .post(format!("{}/sessions", self.base))
            .bearer_auth(&self.token)
            .json(&CreateSession {
                title,
                screen_size,
                shell_timeouts,
                owner,
                idle_secs,
            })
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
        check(response).await.map(|_| ())
    }

    /// The viewer password and how many viewer pages are open.
    pub async fn viewer(&self) -> Result<ViewerInfo> {
        self.http
            .get(format!("{}/viewer", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("asking the computer for its viewer link")?
            .error_for_status()
            .context("asking the computer for its viewer link")?
            .json()
            .await
            .context("reading the viewer link")
    }

    /// Asks every open viewer page to switch to `screen`. The reply counts the open pages.
    pub async fn show_screen(&self, screen: u8) -> Result<ViewerInfo> {
        self.http
            .post(format!("{}/viewer/show/{screen}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("asking the viewer pages to show the screen")?
            .error_for_status()
            .context("asking the viewer pages to show the screen")?
            .json()
            .await
            .context("reading the viewer reply")
    }

    /// Tells `computerd` the owner is alive, which keeps all its sessions.
    pub async fn heartbeat(&self, owner: &OwnerId) -> Result<()> {
        self.http
            .post(format!("{}/owners/{owner}/heartbeat", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("sending a heartbeat")?
            .error_for_status()
            .context("sending a heartbeat")
            .map(|_| ())
    }

    /// Ends every session of the owner. Gives up after a few seconds.
    pub async fn end_owner(&self, owner: &OwnerId) -> Result<()> {
        self.http
            .delete(format!("{}/owners/{owner}", self.base))
            .bearer_auth(&self.token)
            .timeout(SHUTDOWN_TIMEOUT)
            .send()
            .await
            .context("ending the sessions of this server")?
            .error_for_status()
            .context("ending the sessions of this server")
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
        check(response)
            .await?
            .json()
            .await
            .context("reading the screenshot reply")
    }

    /// Opens a file or an http(s) URL on the session's screen and returns a screenshot.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn open_path(&self, session: &SessionId, path: String) -> Result<Observation> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/open", self.base))
            .bearer_auth(&self.token)
            .timeout(LAUNCH_TIMEOUT)
            .json(&OpenPathRequest { path })
            .send()
            .await
            .context("asking the computer to open the path")?;
        read_reply(response, "reading the screenshot reply").await
    }

    /// Starts or raises an application on the session's screen and returns a screenshot.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn launch_app(
        &self,
        session: &SessionId,
        application: String,
        uri: Option<String>,
    ) -> Result<Observation> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/launch", self.base))
            .bearer_auth(&self.token)
            .timeout(LAUNCH_TIMEOUT)
            .json(&LaunchAppRequest { application, uri })
            .send()
            .await
            .context("asking the computer to launch the application")?;
        read_reply(response, "reading the screenshot reply").await
    }

    /// Runs a batch of actions on the session's screen.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn act(&self, session: &SessionId, request: &ActRequest) -> Result<ActReply> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/act", self.base))
            .bearer_auth(&self.token)
            .timeout(request.time_budget() + ACT_MARGIN)
            .json(request)
            .send()
            .await
            .context("sending the actions to the computer")?;
        check(response)
            .await?
            .json()
            .await
            .context("reading the reply to the actions")
    }

    /// Runs a shell command in the session's working folder.
    ///
    /// `timeouts` are the session's, so the HTTP timeout outlasts the command's own.
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn shell(
        &self,
        session: &SessionId,
        request: &ShellRequest,
        timeouts: ShellTimeouts,
    ) -> Result<ShellReply> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/shell", self.base))
            .bearer_auth(&self.token)
            .timeout(timeouts.effective(request.timeout_secs) + SHELL_MARGIN)
            .json(request)
            .send()
            .await
            .context("sending the command to the computer")?;
        read_reply(response, "reading the result of the command").await
    }

    /// Changes the session's working folder.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn set_cwd(&self, session: &SessionId, path: String) -> Result<SetCwdReply> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/cwd", self.base))
            .bearer_auth(&self.token)
            .json(&SetCwdRequest { path })
            .send()
            .await
            .context("asking the computer to change folder")?;
        read_reply(response, "reading the new working folder").await
    }

    /// Lists a folder, the session's working folder when `path` is `None`.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn list_files(
        &self,
        session: &SessionId,
        request: &ListFilesRequest,
    ) -> Result<ListFilesReply> {
        self.file_call(session, "list", request, "listing the folder")
            .await
    }

    /// Reads a text file or an image.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn read_file(
        &self,
        session: &SessionId,
        request: &ReadFileRequest,
    ) -> Result<ReadFileReply> {
        self.file_call(session, "read", request, "reading the file")
            .await
    }

    /// Writes a text file.
    ///
    /// Fails with [`UnknownSession`] when `computerd` does not know the session.
    pub async fn write_file(
        &self,
        session: &SessionId,
        request: &WriteFileRequest,
    ) -> Result<WriteFileReply> {
        self.file_call(session, "write", request, "writing the file")
            .await
    }

    async fn file_call<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        session: &SessionId,
        verb: &str,
        body: &B,
        what: &'static str,
    ) -> Result<T> {
        let response = self
            .http
            .post(format!("{}/sessions/{session}/files/{verb}", self.base))
            .bearer_auth(&self.token)
            .timeout(FILE_TIMEOUT)
            .json(body)
            .send()
            .await
            .with_context(|| format!("sending the request to the computer for {what}"))?;
        read_reply(response, what).await
    }
}

async fn read_reply<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    reading: &'static str,
) -> Result<T> {
    check(response).await?.json().await.context(reading)
}

/// Passes a successful response on. Turns a missing session into [`UnknownSession`] and other failures into the computer's message.
async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let message = match response.json::<ApiError>().await {
        Ok(error) => Some(error.message),
        Err(_) => None,
    };
    match status {
        StatusCode::NOT_FOUND => Err(UnknownSession { reason: None }.into()),
        StatusCode::GONE => Err(UnknownSession { reason: message }.into()),
        _ => bail!(
            "{}",
            message.unwrap_or_else(|| format!("the computer answered {status}"))
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatch_advice_depends_on_which_side_is_newer() {
        let older = protocol_mismatch(PROTOCOL_VERSION - 1, "0.0.1", "box");
        assert!(older.contains("`docker stop box`"), "{older}");
        let newer = protocol_mismatch(PROTOCOL_VERSION + 1, "9.9.9", "box");
        assert!(
            newer.contains("Update the computer-use-mcp binary"),
            "{newer}"
        );
        assert!(!newer.contains("docker stop"), "{newer}");
    }
}
