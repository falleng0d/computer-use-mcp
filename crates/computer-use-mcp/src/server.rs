use std::sync::Arc;

use anyhow::Context;
use computer_protocol::{ScreenSize, SessionId, SessionTitle};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_handler,
    tool_router,
};
use serde::Deserialize;
use tokio::sync::{Mutex, OnceCell};
use tracing::error;

use crate::{
    client::{Client, UnknownSession},
    computer::{Docked, Settings},
    image::{self, Image},
    observation,
};

const START_FIRST: &str = "call start_computer first";
const SCREEN_SIZE_ENV: &str = "COMPUTER_USE_SCREEN_SIZE";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartComputerArgs {
    /// Short description of your task, 1 to 80 characters. Shown to the user next to your screen.
    title: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EndSessionArgs {
    /// Session id returned by `start_computer`.
    session: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ObserveArgs {
    /// Session id returned by `start_computer`.
    session: String,
}

/// Reads the screen size setting. An invalid value is kept as a message for `start_computer`.
fn screen_size_from_env() -> Result<ScreenSize, String> {
    match std::env::var(SCREEN_SIZE_ENV) {
        Ok(text) if !text.is_empty() => {
            ScreenSize::parse(&text).map_err(|error| format!("{SCREEN_SIZE_ENV}={text}: {error}"))
        }
        _ => Ok(ScreenSize::default()),
    }
}

#[derive(Clone)]
pub struct Server {
    settings: Settings,
    screen_size: Result<ScreenSize, String>,
    image: Image,
    docked: Arc<OnceCell<Docked>>,
    start_lock: Arc<Mutex<()>>,
}

impl Server {
    pub fn from_env() -> Self {
        Self::new(
            Settings::from_env(),
            image::from_env(),
            screen_size_from_env(),
        )
    }

    fn new(settings: Settings, image: Image, screen_size: Result<ScreenSize, String>) -> Self {
        Self {
            settings,
            screen_size,
            image,
            docked: Arc::default(),
            start_lock: Arc::default(),
        }
    }

    async fn docked(&self) -> anyhow::Result<&Docked> {
        self.docked
            .get_or_try_init(|| async {
                Docked::connect(self.settings.clone(), self.image.clone())
            })
            .await
    }

    async fn start(&self, title: &str) -> anyhow::Result<SessionId> {
        let title = SessionTitle::parse(title)?;
        let screen_size = self
            .screen_size
            .clone()
            .map_err(|message| anyhow::anyhow!(message))?;
        let docked = self.docked().await?;
        let endpoint = {
            let _starting = self.start_lock.lock().await;
            docked.ensure_running().await?
        };
        let client = Client::new(&endpoint)?;
        client.wait_until_ready(docked.name()).await?;
        client.create_session(title, screen_size).await
    }

    /// Client for the running computer, or the `START_FIRST` error.
    async fn client_for(&self, session: &str) -> anyhow::Result<(SessionId, Client)> {
        let session = SessionId::parse(session).map_err(|_| anyhow::anyhow!(START_FIRST))?;
        let endpoint = self
            .docked()
            .await?
            .running_endpoint()
            .await?
            .ok_or_else(|| anyhow::anyhow!(START_FIRST))?;
        Ok((session, Client::new(&endpoint)?))
    }

    async fn observe(&self, session: &str) -> anyhow::Result<CallToolResult> {
        let (session, client) = self.client_for(session).await?;
        match client.observe(&session).await {
            Err(error) if error.is::<UnknownSession>() => anyhow::bail!(START_FIRST),
            other => other.map(observation::tool_result),
        }
    }

    async fn end(&self, session: &str) -> anyhow::Result<()> {
        let (session, client) = self.client_for(session).await?;
        match client.end_session(&session).await {
            Err(error) if error.is::<UnknownSession>() => anyhow::bail!(START_FIRST),
            other => other,
        }
    }
}

fn report(what: &str, result: anyhow::Result<String>) -> Result<String, String> {
    result.map_err(|error| {
        let message = format!("{error:#}");
        error!(tool = what, error = %message, "tool call failed");
        message
    })
}

#[tool_router]
impl Server {
    #[tool(
        description = "Start your own session on the shared computer and get a session id. Creates or starts the computer if needed. Call this once before using any other tool, and pass the returned session id to them. A subagent that needs its own screen must call this itself instead of reusing its parent's session."
    )]
    async fn start_computer(
        &self,
        Parameters(args): Parameters<StartComputerArgs>,
    ) -> Result<String, String> {
        let result = self
            .start(&args.title)
            .await
            .context("starting the computer")
            .map(|session| format!("session: {session}"));
        report("start_computer", result)
    }

    #[tool(
        description = "End your session when your task is done. Files in the computer's home stay."
    )]
    async fn end_session(
        &self,
        Parameters(args): Parameters<EndSessionArgs>,
    ) -> Result<String, String> {
        let result = self
            .end(&args.session)
            .await
            .map(|()| "session ended".to_owned());
        report("end_session", result)
    }

    #[tool(
        description = "Take a screenshot of your own screen. Each session has its own screen, opened on your first call, so other agents never see or touch yours. A subagent must call start_computer itself to get its own screen and must not reuse its parent's session. Returns a PNG plus the frame id, capture time, size, cursor position, and active window title. When nothing changed since your previous screenshot, the image is left out and the previous one is still valid."
    )]
    async fn computer_observe(
        &self,
        Parameters(args): Parameters<ObserveArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.observe(&args.session).await;
        result.map_err(|error| {
            let message = format!("{error:#}");
            error!(tool = "computer_observe", error = %message, "tool call failed");
            message
        })
    }
}

#[expect(
    clippy::unused_async_trait_impl,
    reason = "the macro generates async handler methods that need no await"
)]
#[tool_handler(
    name = "computer-use-mcp",
    instructions = "Gives you a Linux computer shared with other agents and the user. Call start_computer first and pass its session id to the other tools."
)]
impl rmcp::ServerHandler for Server {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bollard::{
        Docker,
        query_parameters::{
            ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
            StopContainerOptionsBuilder,
        },
    };

    use uuid::Uuid;

    use super::*;

    struct Cleanup {
        docker: Docker,
        name: String,
        volume: String,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let docker = self.docker.clone();
            let (name, volume) = (self.name.clone(), self.volume.clone());
            let _ = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime can always be built");
                runtime.block_on(async {
                    let options = RemoveContainerOptionsBuilder::new().force(true).build();
                    let _ = docker.remove_container(&name, Some(options)).await;
                    let _ = docker
                        .remove_volume(
                            &volume,
                            None::<bollard::query_parameters::RemoveVolumeOptions>,
                        )
                        .await;
                });
            })
            .join();
        }
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn computer_is_created_restarted_and_sessions_end() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: Some("UTC".to_owned()),
        };
        let docker = Docker::connect_with_defaults().unwrap();
        let _cleanup = Cleanup {
            docker: docker.clone(),
            name: name.clone(),
            volume: settings.volume(),
        };
        let server = Server::new(settings, image::from_env(), Ok(ScreenSize::default()));

        let first = server.start("first task").await.unwrap();
        server.end(first.as_str()).await.unwrap();
        let error = server.end(first.as_str()).await.unwrap_err();
        assert_eq!(error.to_string(), START_FIRST);

        let stop = StopContainerOptionsBuilder::new().t(1).build();
        docker.stop_container(&name, Some(stop)).await.unwrap();
        assert_eq!(
            server.end("anything").await.unwrap_err().to_string(),
            START_FIRST
        );

        let second = server.start("second task").await.unwrap();
        assert_ne!(first, second);
        server.end(second.as_str()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn observing_shows_a_screen_then_omits_the_unchanged_frame() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
        };
        let docker = Docker::connect_with_defaults().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(settings, image::from_env(), Ok(ScreenSize::default()));

        let session = server.start("watcher").await.unwrap();
        let other = server.start("other watcher").await.unwrap();
        let content_len = |result: CallToolResult| result.content.len();

        let first = server.observe(session.as_str()).await.unwrap();
        assert_eq!(content_len(first), 2);
        let second = server.observe(session.as_str()).await.unwrap();
        assert_eq!(content_len(second), 1);
        let others_first = server.observe(other.as_str()).await.unwrap();
        assert_eq!(content_len(others_first), 2);

        server.end(session.as_str()).await.unwrap();
        let error = server.observe(session.as_str()).await.unwrap_err();
        assert_eq!(error.to_string(), START_FIRST);
        server.end(other.as_str()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn two_processes_starting_a_fresh_computer_share_one_container() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
        };
        let docker = Docker::connect_with_defaults().unwrap();
        let _cleanup = Cleanup {
            docker: docker.clone(),
            name: name.clone(),
            volume: settings.volume(),
        };
        let one = Server::new(
            settings.clone(),
            image::from_env(),
            Ok(ScreenSize::default()),
        );
        let two = Server::new(settings, image::from_env(), Ok(ScreenSize::default()));

        let (first, second) = tokio::join!(one.start("one"), two.start("two"));
        assert_ne!(first.unwrap(), second.unwrap());

        let filters = HashMap::from([("name".to_owned(), vec![name])]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        assert_eq!(
            docker.list_containers(Some(options)).await.unwrap().len(),
            1
        );
    }
}
