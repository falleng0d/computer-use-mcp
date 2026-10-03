use std::sync::Arc;

use anyhow::Context;
use computer_protocol::SessionTitle;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::sync::{Mutex, OnceCell};
use tracing::error;

use crate::{
    client::{Client, UnknownSession},
    computer::{Docked, Settings},
    image::{self, Image},
};

const START_FIRST: &str = "call start_computer first";

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

#[derive(Clone)]
pub struct Server {
    settings: Settings,
    image: Image,
    docked: Arc<OnceCell<Docked>>,
    start_lock: Arc<Mutex<()>>,
}

impl Server {
    pub fn from_env() -> Self {
        Self::new(Settings::from_env(), image::from_env())
    }

    fn new(settings: Settings, image: Image) -> Self {
        Self {
            settings,
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

    async fn start(&self, title: &str) -> anyhow::Result<String> {
        let title = SessionTitle::parse(title)?;
        let docked = self.docked().await?;
        let endpoint = {
            let _starting = self.start_lock.lock().await;
            docked.ensure_running().await?
        };
        let client = Client::new(&endpoint)?;
        client.wait_until_ready(docked.name()).await?;
        client.create_session(title).await
    }

    /// Client for the running computer, or the `START_FIRST` error.
    async fn client_for(&self, session: &str) -> anyhow::Result<Client> {
        if session.is_empty() {
            anyhow::bail!(START_FIRST);
        }
        let endpoint = self
            .docked()
            .await?
            .running_endpoint()
            .await?
            .ok_or_else(|| anyhow::anyhow!(START_FIRST))?;
        Client::new(&endpoint)
    }

    async fn end(&self, session: &str) -> anyhow::Result<()> {
        let client = self.client_for(session).await?;
        match client.end_session(session).await {
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
    use bollard::{
        Docker,
        query_parameters::{RemoveContainerOptionsBuilder, StopContainerOptionsBuilder},
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
        let server = Server::new(settings, image::from_env());

        let first = server.start("first task").await.unwrap();
        server.end(&first).await.unwrap();
        let error = server.end(&first).await.unwrap_err();
        assert_eq!(error.to_string(), START_FIRST);

        let stop = StopContainerOptionsBuilder::new().t(1).build();
        docker.stop_container(&name, Some(stop)).await.unwrap();
        assert_eq!(
            server.end("anything").await.unwrap_err().to_string(),
            START_FIRST
        );

        let second = server.start("second task").await.unwrap();
        assert_ne!(first, second);
        server.end(&second).await.unwrap();
    }
}
