use std::sync::Arc;

use anyhow::Context;
use computer_protocol::{ActRequest, RawAction, ScreenSize, SessionId, SessionTitle};
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ActArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// Up to 24 actions, run in order on your screen. A double click counts as two.
    actions: Vec<RawAction>,
    /// End with a screenshot of the result. Default true.
    observe: Option<bool>,
    /// Milliseconds to wait before that screenshot so the screen can settle, up to 5000. Default 300.
    settle_ms: Option<f64>,
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

    async fn act(&self, args: ActArgs) -> anyhow::Result<CallToolResult> {
        let request = ActRequest::parse(&args.actions, args.observe, args.settle_ms)?;
        let (session, client) = self.client_for(&args.session).await?;
        match client.act(&session, &request).await {
            Err(error) if error.is::<UnknownSession>() => anyhow::bail!(START_FIRST),
            other => other.map(observation::act_result),
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

    #[tool(
        description = "Act on your own screen with up to 24 ordered actions, optionally ending with a screenshot. Coordinates are pixels from the top left of the latest screenshot. Actions: click {x, y, button?: left|right|middle, double?}, move {x, y}, down/up {x?, y?, button?} to drag, type {text} for any Unicode text (a newline presses Enter), key {key, modifiers?} for keys like enter, esc, tab, backspace, delete, space, arrows, home, end, pageup, pagedown, f1 to f12 or a single character with modifiers ctrl, alt, shift, super (also cmd, option), scroll {x?, y?, direction: up|down|left|right, amount? 1 to 20, default 3}, wait {ms? up to 5000, default 350}, focus {application} to raise an already open window by name or title. Batch only predictable actions and stop before an outcome you need to inspect. By default the batch ends with a screenshot taken settle_ms (default 300) after the last action; set observe to false to skip it. Repeating the same scroll, pointer, or key batch after it changed nothing is refused on the 4th try, so change your approach."
    )]
    async fn computer_act(
        &self,
        Parameters(args): Parameters<ActArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.act(args).await;
        result.map_err(|error| {
            let message = format!("{error:#}");
            error!(tool = "computer_act", error = %message, "tool call failed");
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
        let mut unchanged = false;
        for _ in 0..10 {
            let again = server.observe(session.as_str()).await.unwrap();
            unchanged = content_len(again) == 1;
            if unchanged {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        assert!(unchanged, "the desktop kept changing after it settled");
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

    fn act_args(session: &SessionId, actions: &serde_json::Value) -> ActArgs {
        serde_json::from_value(serde_json::json!({
            "session": session.as_str(),
            "actions": actions,
            "settle_ms": 100,
        }))
        .unwrap()
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn acting_changes_the_screen_and_a_repeated_idle_batch_is_refused() {
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
        let session = server.start("actor").await.unwrap();

        let menu = server
            .act(act_args(
                &session,
                &serde_json::json!([{ "kind": "click", "x": 600, "y": 400, "button": "right" }]),
            ))
            .await
            .unwrap();
        assert_eq!(
            menu.content.len(),
            3,
            "the opened menu is a new frame with an image"
        );

        let off_screen = server
            .act(act_args(
                &session,
                &serde_json::json!([{ "kind": "click", "x": 5000, "y": 1 }]),
            ))
            .await
            .unwrap_err();
        assert!(off_screen.to_string().contains("1280x800"), "{off_screen}");

        let scroll = || {
            act_args(
                &session,
                &serde_json::json!([{ "kind": "scroll", "direction": "down", "x": 900, "y": 600 }]),
            )
        };
        let mut allowed = 0;
        let mut refusal = None;
        for _ in 0..10 {
            match server.act(scroll()).await {
                Ok(_) => allowed += 1,
                Err(error) => {
                    refusal = Some(error);
                    break;
                }
            }
        }
        let refusal = refusal.expect("a repeated idle batch is refused");
        assert!(allowed >= 3, "refused after only {allowed} runs");
        assert!(
            refusal.to_string().contains("Change your approach"),
            "{refusal}"
        );

        server
            .act(act_args(
                &session,
                &serde_json::json!([{ "kind": "type", "text": "é→" }]),
            ))
            .await
            .unwrap();
        server.act(scroll()).await.unwrap();
        server.end(session.as_str()).await.unwrap();
    }
}
