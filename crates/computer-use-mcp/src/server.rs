use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::Context;
use computer_protocol::{
    ActRequest, HEARTBEAT_INTERVAL_SECS, ListFilesRequest, OwnerId, RawAction, ReadFileRequest,
    ScreenSize, SessionId, SessionTitle, ShellRequest, ShellTimeouts, WriteFileRequest,
};
use rmcp::{
    RoleServer, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    service::RequestContext, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::{
    sync::{Mutex, OnceCell, watch},
    task::JoinHandle,
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    client::{Client, UnknownSession},
    computer::{Docked, Endpoint, Settings},
    file_result,
    image::{self, Image},
    observation,
    open::{self, Opener},
    settings, shell_result, transfer,
};

const SESSION_UNAVAILABLE: &str = "the computer is not running or this session id is not valid, call start_computer to get a new session";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(HEARTBEAT_INTERVAL_SECS);

/// Message for a call on a session that ended, or that `computerd` never knew.
fn gone(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<UnknownSession>() {
        Some(UnknownSession {
            reason: Some(reason),
        }) => anyhow::anyhow!("{reason}, call start_computer to get a new session"),
        Some(_) => anyhow::anyhow!(SESSION_UNAVAILABLE),
        None => error,
    }
}

/// Sends heartbeats for the sessions of this process and ends them at shutdown.
///
/// Nothing runs until the first session exists, so a process that never starts a computer never calls Docker or HTTP.
struct Heartbeats {
    owner: OwnerId,
    endpoint: watch::Sender<Option<Endpoint>>,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl Heartbeats {
    fn new() -> Self {
        let owner = OwnerId::parse(&Uuid::new_v4().simple().to_string())
            .expect("a simple UUID is 32 lowercase hex digits");
        Self {
            owner,
            endpoint: watch::channel(None).0,
            task: StdMutex::new(None),
        }
    }

    /// Starts the heartbeat task if needed and points it at the computer's current endpoint.
    fn start(&self, endpoint: &Endpoint) {
        self.endpoint.send_replace(Some(endpoint.clone()));
        let mut task = self
            .task
            .lock()
            .expect("the heartbeat task lock is only held to start or stop it");
        if task.is_none() {
            let owner = self.owner.clone();
            let endpoint = self.endpoint.subscribe();
            *task = Some(tokio::spawn(beat(owner, endpoint)));
        }
    }

    /// Stops the heartbeats and ends this process's sessions, giving up after a few seconds.
    async fn stop(&self) {
        let task = self
            .task
            .lock()
            .expect("the heartbeat task lock is only held to start or stop it")
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        let endpoint = self.endpoint.borrow().clone();
        if let Some(endpoint) = endpoint {
            let ended = async { Client::new(&endpoint)?.end_owner(&self.owner).await }.await;
            if let Err(error) = ended {
                error!(error = %format!("{error:#}"), "could not end the sessions at shutdown");
            }
        }
    }
}

async fn beat(owner: OwnerId, endpoint: watch::Receiver<Option<Endpoint>>) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    let mut client: Option<(Endpoint, Client)> = None;
    let mut failing = false;
    loop {
        tick.tick().await;
        let current = endpoint.borrow().clone();
        let Some(current) = current else { continue };
        if client.as_ref().is_none_or(|(known, _)| *known != current) {
            match Client::new(&current) {
                Ok(new) => client = Some((current, new)),
                Err(error) => {
                    warn!(error = %format!("{error:#}"), "could not build the heartbeat client");
                    continue;
                }
            }
        }
        let Some((_, client)) = &client else { continue };
        match client.heartbeat(&owner).await {
            Err(error) if !failing => {
                failing = true;
                warn!(error = %format!("{error:#}"), "heartbeats to the computer are failing, its sessions end 30 s after the last one arrived");
            }
            Ok(()) if failing => {
                failing = false;
                info!("heartbeats to the computer work again");
            }
            Err(_) | Ok(()) => {}
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StartComputerArgs {
    /// Short description of your task, 1 to 80 characters. Shown to the user next to your screen.
    title: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EndSessionArgs {
    /// Session id returned by `start_computer`.
    session: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ObserveArgs {
    /// Session id returned by `start_computer`.
    session: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// Up to 24 actions, run in order on your screen. A double click counts as two.
    actions: Vec<RawAction>,
    /// End with a screenshot of the result. Default true.
    observe: Option<bool>,
    /// Milliseconds to wait before that screenshot so the screen can settle, up to 5000. Default 300.
    settle_ms: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ShellArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// Command line, run with `bash -lc` in your working folder.
    command: String,
    /// Seconds the command may run before it is killed. Defaults to the computer's setting, 120 s unless changed, and is capped at its maximum, 600 s unless changed.
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenPathArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// An http(s) URL, or a file path. A relative path starts at your working folder, `~` is home.
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LaunchAppArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// What to launch: `browser`, `terminal`, the name of an installed application, or a program on `PATH`.
    application: String,
    /// A page or file the application should open, when it takes one.
    uri: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetCwdArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// Folder that becomes your working folder. A relative path starts at the current one, `~` is home.
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListFilesArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// Folder to list. A relative path starts at your working folder, `~` is home. Defaults to your working folder.
    path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadFileArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// File to read. A relative path starts at your working folder, `~` is home.
    path: String,
    /// First line to return, counting from 1. Text files only. Default 1.
    offset: Option<u64>,
    /// Number of lines to return. Text files only. Default: as many as fit.
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteFileArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// File to write. A relative path starts at your working folder, `~` is home. Missing folders are created.
    path: String,
    /// UTF-8 text that becomes the whole content of the file, up to 10 MB.
    content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FileTransferArgs {
    /// Session id returned by `start_computer`.
    session: String,
    /// `to_computer` copies from the host to the computer. `from_computer` copies from the computer to the host.
    direction: transfer::Direction,
    /// Path on the host, the machine the user works on. Absolute, `~` for the host user's home folder (for example `~/Downloads`), or relative to the MCP server's working folder.
    host_path: String,
    /// Path on the computer. A relative path starts at your working folder, `~` is `/home/computer`.
    computer_path: String,
    /// Replace files that already exist. Default false, which refuses when the destination exists. A folder merges into an existing folder, replacing same-named files and keeping the rest.
    overwrite: Option<bool>,
}

#[derive(Clone)]
pub(crate) struct Server {
    settings: Settings,
    screen_size: Result<ScreenSize, String>,
    shell_timeouts: Result<ShellTimeouts, String>,
    idle: Result<NonZeroU32, String>,
    open_mode: Result<open::Mode, String>,
    opener: Arc<Opener>,
    heartbeats: Arc<Heartbeats>,
    image: Image,
    docked: Arc<OnceCell<Docked>>,
    start_lock: Arc<Mutex<()>>,
}

/// What `start_computer` hands back.
struct Started {
    session: SessionId,
    /// Link that opens the viewer page, when the computer has one.
    link: Option<String>,
}

impl Started {
    fn describe(&self) -> String {
        match &self.link {
            Some(link) => format!(
                "session: {}\nviewer: {link}\nThe viewer link opens a page where the user can watch and use every screen. Tell the user about it. The password in the link is private to the user.",
                self.session
            ),
            None => format!("session: {}", self.session),
        }
    }
}

impl Server {
    pub(crate) fn from_env() -> Self {
        Self::new(
            Settings::from_env(),
            image::from_env(),
            settings::screen_size(),
            settings::shell_timeouts(),
            settings::idle(),
            settings::open_mode(),
        )
    }

    fn new(
        settings: Settings,
        image: Image,
        screen_size: Result<ScreenSize, String>,
        shell_timeouts: Result<ShellTimeouts, String>,
        idle: Result<NonZeroU32, String>,
        open_mode: Result<open::Mode, String>,
    ) -> Self {
        let opener = Arc::new(Opener::new(open_mode.clone().unwrap_or_default()));
        Self {
            settings,
            screen_size,
            shell_timeouts,
            idle,
            open_mode,
            opener,
            heartbeats: Arc::new(Heartbeats::new()),
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

    async fn start(&self, title: &str) -> anyhow::Result<Started> {
        let title = SessionTitle::parse(title)?;
        let screen_size = self
            .screen_size
            .clone()
            .map_err(|message| anyhow::anyhow!(message))?;
        let shell_timeouts = self.shell_timeouts()?;
        let idle_secs = self
            .idle
            .clone()
            .map_err(|message| anyhow::anyhow!(message))?;
        self.open_mode
            .clone()
            .map_err(|message| anyhow::anyhow!(message))?;
        let docked = self.docked().await?;
        let endpoint = {
            let _starting = self.start_lock.lock().await;
            Box::pin(docked.ensure_running()).await?
        };
        let client = Client::new(&endpoint)?;
        client.wait_until_ready(docked.name()).await?;
        let session = client
            .create_session(
                title,
                screen_size,
                shell_timeouts,
                self.heartbeats.owner.clone(),
                idle_secs,
            )
            .await?;
        self.heartbeats.start(&endpoint);
        let link = match client.viewer().await {
            Ok(info) => endpoint
                .viewer_port
                .map(|port| computer_protocol::viewer_link(port, &info.key)),
            Err(error) => {
                warn!(error = %format!("{error:#}"), "could not read the viewer link");
                None
            }
        };
        Ok(Started { session, link })
    }

    /// Ends every session this process started. Call before the process exits.
    pub(crate) async fn shutdown(&self) {
        self.opener.shutdown().await;
        self.heartbeats.stop().await;
    }

    /// Client for the running computer, or the `SESSION_UNAVAILABLE` error.
    async fn client_for(&self, session: &str) -> anyhow::Result<(SessionId, Client)> {
        let (session, endpoint) = self.endpoint_for(session).await?;
        Ok((session, Client::new(&endpoint)?))
    }

    /// Endpoint of the running computer, or the `SESSION_UNAVAILABLE` error.
    async fn endpoint_for(&self, session: &str) -> anyhow::Result<(SessionId, Endpoint)> {
        let session =
            SessionId::parse(session).map_err(|_| anyhow::anyhow!(SESSION_UNAVAILABLE))?;
        let endpoint = self
            .docked()
            .await?
            .running_endpoint()
            .await?
            .ok_or_else(|| anyhow::anyhow!(SESSION_UNAVAILABLE))?;
        Ok((session, endpoint))
    }

    async fn observe(&self, session: &str) -> anyhow::Result<CallToolResult> {
        let (session, endpoint) = self.endpoint_for(session).await?;
        match Client::new(&endpoint)?.observe(&session).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|observation| {
                self.announce(&endpoint, observation.opened_screen);
                observation::tool_result(observation)
            }),
        }
    }

    async fn act(&self, args: ActArgs) -> anyhow::Result<CallToolResult> {
        let request = ActRequest::parse(&args.actions, args.observe, args.settle_ms)?;
        let (session, endpoint) = self.endpoint_for(&args.session).await?;
        match Client::new(&endpoint)?.act(&session, &request).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|reply| {
                self.announce(&endpoint, reply.opened_screen);
                observation::act_result(reply)
            }),
        }
    }

    async fn open(&self, args: OpenPathArgs) -> anyhow::Result<CallToolResult> {
        let (session, endpoint) = self.endpoint_for(&args.session).await?;
        match Client::new(&endpoint)?.open_path(&session, args.path).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|observation| {
                self.announce(&endpoint, observation.opened_screen);
                observation::tool_result(observation)
            }),
        }
    }

    async fn launch(&self, args: LaunchAppArgs) -> anyhow::Result<CallToolResult> {
        let (session, endpoint) = self.endpoint_for(&args.session).await?;
        let client = Client::new(&endpoint)?;
        match client
            .launch_app(&session, args.application, args.uri)
            .await
        {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|observation| {
                self.announce(&endpoint, observation.opened_screen);
                observation::tool_result(observation)
            }),
        }
    }

    /// Opens the viewer when the call opened a screen, without waiting for it.
    fn announce(&self, endpoint: &Endpoint, opened_screen: Option<u8>) {
        if let Some(screen) = opened_screen {
            self.opener.screen_opened(endpoint.clone(), screen);
        }
    }

    fn shell_timeouts(&self) -> anyhow::Result<ShellTimeouts> {
        self.shell_timeouts
            .clone()
            .map_err(|message| anyhow::anyhow!(message))
    }

    async fn run_shell(&self, args: ShellArgs) -> anyhow::Result<CallToolResult> {
        let (session, client) = self.client_for(&args.session).await?;
        let request = ShellRequest {
            command: args.command,
            timeout_secs: args.timeout,
        };
        match client
            .shell(&session, &request, self.shell_timeouts()?)
            .await
        {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|reply| shell_result::tool_result(&reply)),
        }
    }

    async fn change_cwd(&self, args: SetCwdArgs) -> anyhow::Result<String> {
        let (session, client) = self.client_for(&args.session).await?;
        match client.set_cwd(&session, args.path).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|reply| format!("working folder: {}", reply.cwd)),
        }
    }

    async fn files_list(&self, args: ListFilesArgs) -> anyhow::Result<String> {
        let (session, client) = self.client_for(&args.session).await?;
        let request = ListFilesRequest { path: args.path };
        match client.list_files(&session, &request).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|reply| file_result::describe_list(&reply)),
        }
    }

    async fn files_read(&self, args: ReadFileArgs) -> anyhow::Result<CallToolResult> {
        let (session, client) = self.client_for(&args.session).await?;
        let request = ReadFileRequest {
            path: args.path,
            offset: args.offset,
            limit: args.limit,
        };
        match client.read_file(&session, &request).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(file_result::read_result),
        }
    }

    async fn files_write(&self, args: WriteFileArgs) -> anyhow::Result<String> {
        let (session, client) = self.client_for(&args.session).await?;
        let request = WriteFileRequest {
            path: args.path,
            content: args.content,
        };
        match client.write_file(&session, &request).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|reply| file_result::describe_write(&reply)),
        }
    }

    async fn files_transfer(&self, args: FileTransferArgs) -> anyhow::Result<String> {
        let cwd =
            std::env::current_dir().context("finding the working folder of the MCP server")?;
        let host = transfer::host_path(&args.host_path, std::env::home_dir().as_deref(), &cwd)
            .map_err(|message| anyhow::anyhow!(message))?;
        let (session, client) = self.client_for(&args.session).await?;
        let overwrite = args.overwrite.unwrap_or(false);
        let started = std::time::Instant::now();
        let copied = match args.direction {
            transfer::Direction::ToComputer => {
                transfer::to_computer(&client, &session, host, args.computer_path, overwrite).await
            }
            transfer::Direction::FromComputer => {
                transfer::from_computer(&client, &session, args.computer_path, host, overwrite)
                    .await
            }
        };
        match copied {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other.map(|reply| transfer::describe(&reply, started.elapsed())),
        }
    }

    async fn end(&self, session: &str) -> anyhow::Result<()> {
        let (session, client) = self.client_for(session).await?;
        match client.end_session(&session).await {
            Err(error) if error.is::<UnknownSession>() => Err(gone(error)),
            other => other,
        }
    }
}

fn report<T>(what: &str, result: anyhow::Result<T>) -> Result<T, String> {
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
            .map(|started| started.describe());
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
        report("computer_observe", result)
    }

    #[tool(
        description = "Act on your own screen with up to 24 ordered actions, optionally ending with a screenshot. Coordinates are pixels from the top left of the latest screenshot. Actions: click {x, y, button?: left|right|middle, double?}, move {x, y}, down/up {x?, y?, button?} to drag, type {text} for any Unicode text (a newline presses Enter), key {key, modifiers?} for keys like enter, esc, tab, backspace, delete, space, arrows, home, end, pageup, pagedown, f1 to f12 or a single character with modifiers ctrl, alt, shift, super (also cmd, option), scroll {x?, y?, direction: up|down|left|right, amount? 1 to 20, default 3}, wait {ms? up to 5000, default 350}, focus {application, uri?} to raise an open window by name or title, or start the application (and open the uri in it) when none matches. Batch only predictable actions and stop before an outcome you need to inspect. By default the batch ends with a screenshot taken settle_ms (default 300) after the last action; set observe to false to skip it. Repeating the same scroll, pointer, or key batch after it changed nothing is refused on the 4th try, so change your approach."
    )]
    async fn computer_act(
        &self,
        Parameters(args): Parameters<ActArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.act(args).await;
        report("computer_act", result)
    }

    #[tool(
        description = "Open a file or a web page on your screen and get a screenshot of the result. An http(s) URL opens in a new tab of your screen's Chromium, which blocks ads with uBlock Origin Lite. A path (relative paths start at your working folder, `~` is home) opens with its default application. HTML, PDF, image, text, JSON and XML files open in Chromium. Opens your screen on your first call. The page may still be loading, so call computer_observe again if the screenshot looks unfinished."
    )]
    async fn open_path(
        &self,
        Parameters(args): Parameters<OpenPathArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.open(args).await;
        report("open_path", result)
    }

    #[tool(
        description = "Start an application on your screen, or raise it when a window of it is already open, then get a screenshot. `application` is `browser` (your screen's Chromium), `terminal`, the name of an installed application, or a program on PATH. `uri` is a page or file for the application to open, and it always starts or reuses the application with it. Opens your screen on your first call."
    )]
    async fn launch_app(
        &self,
        Parameters(args): Parameters<LaunchAppArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.launch(args).await;
        report("launch_app", result)
    }

    #[tool(
        description = "Run a shell command on the computer with `bash -lc`, as the user `computer`, in your working folder (home until you call set_cwd). Returns the exit code, how long it ran, then stdout and stderr separately. A non-zero exit code is a normal result. The call returns when the command exits, so start servers and other long-running jobs in the background with their output redirected, for example `setsid nohup npm run dev >/tmp/dev.log 2>&1 &`. A job started that way keeps running, and without `setsid` it is killed if the command times out. Commands cannot read input. When your session has a screen, DISPLAY points at it, so GUI programs open where you look. A command that runs longer than `timeout` seconds is killed together with everything it started, and the reply says so and holds the output printed until then. Output over about 30000 bytes keeps its start and end with a marker in between, so write big output to a file. Several commands may run at once, and they never wait for your desktop actions."
    )]
    async fn shell(
        &self,
        Parameters(args): Parameters<ShellArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.run_shell(args).await;
        report("shell", result)
    }

    #[tool(
        description = "Set the working folder of your session. Your shell commands start there, and relative paths in file tools resolve from it. A relative path starts at the current working folder, `~` is home, and the folder must exist. Starts at home. Returns the new absolute path."
    )]
    async fn set_cwd(&self, Parameters(args): Parameters<SetCwdArgs>) -> Result<String, String> {
        let result = self.change_cwd(args).await;
        report("set_cwd", result)
    }

    #[tool(
        description = "List the files and folders in one folder of the computer, without going into subfolders. Defaults to your working folder. Relative paths start at your working folder, `~` is home, absolute paths work anywhere the user `computer` can read. Returns the absolute path, then one line per entry with its type (folder, file, symlink), size in bytes, modified time, and name, folders first. Shows up to 1000 entries and says how many were left out. All sessions see the same files."
    )]
    async fn list_files(
        &self,
        Parameters(args): Parameters<ListFilesArgs>,
    ) -> Result<String, String> {
        let result = self.files_list(args).await;
        report("list_files", result)
    }

    #[tool(
        description = "Read a file on the computer. UTF-8 text comes back as text. PNG and JPEG files come back as images (up to 1 MB), so you can look at screenshots and pictures. Other binary files are refused, inspect them with the shell tool or copy them to the host with file_transfer. Relative paths start at your working folder, `~` is home. Long text keeps its start and end with a marker in between and a note on how to read the rest. Use offset and limit, counted in lines from 1, to read a range."
    )]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, String> {
        let result = self.files_read(args).await;
        report("read_file", result)
    }

    #[tool(
        description = "Write a UTF-8 text file on the computer as the user `computer`, replacing the file if it exists. Missing folders are created. The write is atomic, so nobody reads half a file, an existing file keeps its permissions, and a read-only file is refused with permission denied. Relative paths start at your working folder, `~` is home. Content is limited to 10 MB, use file_transfer for binary or larger files. Returns the absolute path and the bytes written. All sessions see the same files."
    )]
    async fn write_file(
        &self,
        Parameters(args): Parameters<WriteFileArgs>,
    ) -> Result<String, String> {
        let result = self.files_write(args).await;
        report("write_file", result)
    }

    #[tool(
        description = "Copy a file or a whole folder between the host (the machine the user works on) and the computer, in either direction. Use it to put something you made on the computer into the user's `~/Downloads`, or to bring a host file or folder onto the computer to work on. `direction` is `to_computer` or `from_computer`. `host_path` is absolute, `~` for the host user's home folder, or relative to the MCP server's working folder. `computer_path` is a path on the computer (relative paths start at your working folder, `~` is /home/computer). Works for any file, binary or text, with no size limit, and folders keep their contents, empty folders, executable bits and symlinks. The destination rule is the one of `cp -r`. When the destination is an existing folder, the source goes inside it under its own name, so a `report.pdf` sent to `~/Downloads` becomes `~/Downloads/report.pdf`. Otherwise the destination is the new name, and missing parent folders are created. By default the call refuses when the final path already exists, and says which path. With `overwrite` true it replaces files, and a folder merges, replacing same-named files and keeping the others. A file never replaces a folder or the other way round. With overwrite, a conflict found partway through a folder stops the copy. Files already copied stay, and the error says how far it got. Names the host cannot hold (on Windows, reserved names and characters such as `:` and `?`) and special files are skipped, and the reply lists them. The reply gives the final destination path, files, folders, bytes, and the time taken. Large transfers keep running while data moves, and fail when nothing moves for 60 seconds. Use this instead of write_file or read_file for binary or large data."
    )]
    async fn file_transfer(
        &self,
        Parameters(args): Parameters<FileTransferArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let result = tokio::select! {
            result = self.files_transfer(args) => result,
            () = context.ct.cancelled() => Err(anyhow::anyhow!("the transfer was cancelled")),
        };
        report("file_transfer", result)
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
mod support;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bollard::query_parameters::{ListContainersOptionsBuilder, StopContainerOptionsBuilder};

    use uuid::Uuid;

    use super::support::{
        Cleanup, Tags, docker_cli, rfb_authenticate, rfb_connect, rfb_size, viewers_on_page,
    };
    use super::*;
    use crate::settings::parse_idle;

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn computer_is_created_restarted_and_sessions_end() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: Some("UTC".to_owned()),
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker: docker.clone(),
            name: name.clone(),
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );

        let first = server.start("first task").await.unwrap().session;
        server.end(first.as_str()).await.unwrap();
        let error = server.end(first.as_str()).await.unwrap_err();
        assert_eq!(error.to_string(), SESSION_UNAVAILABLE);

        let stop = StopContainerOptionsBuilder::new().t(1).build();
        docker.stop_container(&name, Some(stop)).await.unwrap();
        assert_eq!(
            server.end("anything").await.unwrap_err().to_string(),
            SESSION_UNAVAILABLE
        );

        let second = server.start("second task").await.unwrap().session;
        assert_ne!(first, second);
        server.end(second.as_str()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn shutting_down_ends_the_sessions_and_later_calls_say_why() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );

        let session = server.start("leaving").await.unwrap().session;
        server.observe(session.as_str()).await.unwrap();
        server.shutdown().await;

        let message = server
            .observe(session.as_str())
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            message,
            "this session ended because the MCP server that started it shut down, call start_computer to get a new session"
        );
    }

    fn fresh_server() -> (Server, Cleanup) {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );
        (server, cleanup)
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn four_first_calls_at_once_on_a_fresh_computer_all_open_a_screen() {
        let (server, _cleanup) = fresh_server();
        let mut sessions = Vec::new();
        for title in ["a", "b", "c", "d"] {
            sessions.push(server.start(title).await.unwrap().session);
        }
        let results = futures_util::future::join_all(
            sessions
                .iter()
                .map(|session| server.observe(session.as_str())),
        )
        .await;
        for result in results {
            result.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn a_screen_opened_by_a_rejected_call_is_reported_by_the_next_reply() {
        let (server, _cleanup) = fresh_server();
        let session = server.start("rejected first").await.unwrap().session;
        let (_, endpoint) = server.endpoint_for(session.as_str()).await.unwrap();
        let client = Client::new(&endpoint).unwrap();
        let act = |action: serde_json::Value| {
            ActRequest::parse(
                &[serde_json::from_value(action).unwrap()],
                Some(false),
                None,
            )
            .unwrap()
        };
        let refused = client
            .act(
                &session,
                &act(serde_json::json!({"kind": "key", "key": "nosuchkey"})),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{refused:#}").contains("unknown key"),
            "{refused:#}"
        );
        let reply = client
            .act(
                &session,
                &act(serde_json::json!({"kind": "wait", "ms": 10})),
            )
            .await
            .unwrap();
        assert!(reply.opened_screen.is_some());
        let again = client
            .act(
                &session,
                &act(serde_json::json!({"kind": "wait", "ms": 10})),
            )
            .await
            .unwrap();
        assert_eq!(again.opened_screen, None);
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn observing_shows_a_screen_then_omits_the_unchanged_frame() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );

        let session = server.start("watcher").await.unwrap().session;
        let other = server.start("other watcher").await.unwrap().session;
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
        assert_eq!(error.to_string(), SESSION_UNAVAILABLE);
        server.end(other.as_str()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn two_processes_starting_a_fresh_computer_share_one_container() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker: docker.clone(),
            name: name.clone(),
            volume: settings.volume(),
        };
        let one = Server::new(
            settings.clone(),
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );
        let two = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );

        let (first, second) = tokio::join!(one.start("one"), two.start("two"));
        assert_ne!(first.unwrap().session, second.unwrap().session);

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
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );
        let session = server.start("actor").await.unwrap().session;

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

    fn text_of(result: &CallToolResult) -> String {
        serde_json::to_value(&result.content[0]).unwrap()["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn shell_args(session: &SessionId, command: &str, timeout: Option<u64>) -> ShellArgs {
        serde_json::from_value(serde_json::json!({
            "session": session.as_str(),
            "command": command,
            "timeout": timeout,
        }))
        .unwrap()
    }

    /// Longer than the 8 s Chromium gets to quit, since screens close in a background task.
    const CLOSE_WAIT: Duration = Duration::from_secs(15);

    fn open_args(session: &SessionId, path: &str) -> OpenPathArgs {
        serde_json::from_value(serde_json::json!({
            "session": session.as_str(),
            "path": path,
        }))
        .unwrap()
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn opening_a_page_shows_chromium_and_ending_the_session_closes_it_cleanly() {
        let (server, _cleanup) = fresh_server();
        let page = server.start("page").await.unwrap().session;
        let watcher = server.start("watcher").await.unwrap().session;
        let shell = async |command: &str| {
            let result = server
                .run_shell(shell_args(&watcher, command, Some(30)))
                .await
                .unwrap();
            text_of(&result)
        };

        let opened = server
            .open(open_args(&page, "https://example.com"))
            .await
            .unwrap();
        assert!(
            text_of(&opened).contains("Chromium"),
            "{}",
            text_of(&opened)
        );
        assert!(shell("pgrep -x chromium").await.contains("exit code: 0"));
        let profile = "/tmp/computer-use/chromium/screen-1";
        assert!(
            shell(&format!("test -d {profile}"))
                .await
                .contains("exit code: 0")
        );

        let launch = |uri: &str| {
            serde_json::from_value::<LaunchAppArgs>(serde_json::json!({
                "session": page.as_str(),
                "application": "browser",
                "uri": uri,
            }))
            .unwrap()
        };
        for refused in [
            "javascript:alert(1)",
            "chrome://policy",
            "file:///etc/passwd",
        ] {
            let error = server.launch(launch(refused)).await.unwrap_err();
            assert!(error.to_string().contains(refused), "{error:#}");
        }

        let ended = std::time::Instant::now();
        server.end(page.as_str()).await.unwrap();
        let check = format!("pgrep -c chromium; ls -A {profile} | grep -c '^Singleton'; true");
        let mut after = shell(&check).await;
        while !after.contains("--- stdout ---\n0\n0\n") && ended.elapsed() < CLOSE_WAIT {
            tokio::time::sleep(Duration::from_millis(250)).await;
            after = shell(&check).await;
        }
        eprintln!("screen closed {} ms after end", ended.elapsed().as_millis());
        assert!(after.contains("--- stdout ---\n0\n0\n"), "{after}");
    }

    const COOKIE_SERVER: &str = r"cat > /tmp/cookies.py <<'EOF'
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        if self.path == '/set':
            self.send_header('Set-Cookie', 'a=1; HttpOnly; Max-Age=86400; Path=/')
            self.send_header('Set-Cookie', 's=2; Path=/')
            self.send_header('Set-Cookie', 'p=3; Secure; Partitioned; SameSite=None; Path=/')
        if self.path == '/clear':
            self.send_header('Set-Cookie', 'a=; Max-Age=0; Path=/')
        self.send_header('Content-Type', 'text/html')
        self.end_headers()
        self.wfile.write(b'ok')
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(('127.0.0.1', 8099), H).serve_forever()
EOF
cat > /tmp/cookies.js <<'EOF'
const port = 9221 + Number(process.argv[2]);
(async () => {
  const v = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
  const ws = new WebSocket(v.webSocketDebuggerUrl);
  ws.onopen = () => ws.send(JSON.stringify({ id: 1, method: 'Storage.getCookies' }));
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id === 1) {
      const names = m.result.cookies.map((c) => c.name + (c.httpOnly ? '!' : '') + (c.partitionKey ? '#' : ''));
      console.log('names=' + names.sort().join(','));
      process.exit(0);
    }
  };
})();
EOF
setsid python3 /tmp/cookies.py >/dev/null 2>&1 </dev/null &
sleep 1";

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn cookies_follow_the_user_across_screens_and_into_screens_opened_later() {
        let (server, _cleanup) = fresh_server();
        let first = server.start("first").await.unwrap().session;
        let second = server.start("second").await.unwrap().session;
        let shell = async |session: &SessionId, command: &str| {
            let result = server
                .run_shell(shell_args(session, command, Some(30)))
                .await
                .unwrap();
            text_of(&result)
        };
        shell(&first, COOKIE_SERVER).await;
        let cookies_on =
            async |screen: u8| shell(&second, &format!("node /tmp/cookies.js {screen}")).await;
        let wait_for = async |screen: u8, wanted: &str| {
            let started = std::time::Instant::now();
            let mut seen = cookies_on(screen).await;
            while !seen.contains(wanted) && started.elapsed() < Duration::from_secs(20) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                seen = cookies_on(screen).await;
            }
            assert!(
                seen.contains(wanted),
                "screen {screen} has {seen}, wanted {wanted}"
            );
        };

        server
            .open(open_args(&second, "http://127.0.0.1:8099/blank"))
            .await
            .unwrap();
        server
            .open(open_args(&first, "http://127.0.0.1:8099/set"))
            .await
            .unwrap();
        wait_for(2, "names=a!,p#,s\n").await;

        server
            .open(open_args(&first, "http://127.0.0.1:8099/clear"))
            .await
            .unwrap();
        wait_for(2, "names=p#,s\n").await;

        let third = server.start("third").await.unwrap().session;
        server
            .open(open_args(&third, "http://127.0.0.1:8099/blank"))
            .await
            .unwrap();
        assert!(
            cookies_on(3).await.contains("names=p#,s\n"),
            "a screen opened later starts with the jar"
        );

        for session in [&first, &second, &third] {
            server.end(session.as_str()).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn shell_runs_in_the_session_folder_and_a_timeout_leaves_nothing_behind() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let timeouts = ShellTimeouts::new(60, 120).unwrap();
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(timeouts),
            parse_idle(None),
            Ok(open::Mode::None),
        );
        let session = server.start("shell user").await.unwrap().session;
        let run = |command: &'static str, timeout| {
            let args = shell_args(&session, command, timeout);
            async { server.run_shell(args).await.map(|result| text_of(&result)) }
        };
        let cwd = |path: &str| {
            server.change_cwd(
                serde_json::from_value(
                    serde_json::json!({ "session": session.as_str(), "path": path }),
                )
                .unwrap(),
            )
        };

        let mixed = run("echo hi; echo err >&2; exit 3", None).await.unwrap();
        assert!(mixed.starts_with("exit code: 3"), "{mixed}");
        assert!(
            mixed.contains(
                "--- stdout ---
hi
"
            ),
            "{mixed}"
        );
        assert!(
            mixed.contains(
                "--- stderr ---
err
"
            ),
            "{mixed}"
        );

        assert_eq!(cwd("/tmp").await.unwrap(), "working folder: /tmp");
        assert!(run("pwd", None).await.unwrap().contains(
            "--- stdout ---
/tmp
"
        ));
        let missing = cwd("/does/not/exist").await.unwrap_err();
        assert!(missing.to_string().contains("/does/not/exist"), "{missing}");
        assert!(run("pwd", None).await.unwrap().contains(
            "
/tmp
"
        ));

        let before_screen = run("echo \"[$DISPLAY]\"", None).await.unwrap();
        assert!(before_screen.contains("[]"), "{before_screen}");
        server.observe(session.as_str()).await.unwrap();
        let with_screen = run("echo \"[$DISPLAY]\"", None).await.unwrap();
        assert!(!with_screen.contains("[]"), "{with_screen}");

        let started = std::time::Instant::now();
        let slow = run("echo begun; sleep 1000 & wait", Some(2)).await;
        let text = slow.unwrap();
        assert!(text.starts_with("timed out after 2 s"), "{text}");
        assert!(text.contains("begun"), "{text}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let left = run("pgrep -x sleep || echo none", None).await.unwrap();
        assert!(
            left.contains(
                "--- stdout ---
none
"
            ),
            "{left}"
        );

        server.end(session.as_str()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn files_are_shared_between_sessions_and_a_shell_made_png_reads_back_as_an_image() {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );
        let writer = server.start("writer").await.unwrap().session;
        let reader = server.start("reader").await.unwrap().session;
        let write = |session: &SessionId, path: &str, content: &str| {
            let args = serde_json::from_value(serde_json::json!({
                "session": session.as_str(), "path": path, "content": content,
            }))
            .unwrap();
            server.files_write(args)
        };
        let read = |session: &SessionId, path: &str| {
            let args = serde_json::from_value(
                serde_json::json!({ "session": session.as_str(), "path": path }),
            )
            .unwrap();
            server.files_read(args)
        };

        let written = write(&writer, "~/deep/er/note.txt", "h\u{e9}llo\n")
            .await
            .unwrap();
        assert_eq!(written, "wrote 7 bytes to /home/computer/deep/er/note.txt");
        let seen = read(&reader, "/home/computer/deep/er/note.txt")
            .await
            .unwrap();
        assert_eq!(text_of(&seen), "h\u{e9}llo\n");

        server
            .change_cwd(
                serde_json::from_value(
                    serde_json::json!({ "session": writer.as_str(), "path": "deep" }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let relative = read(&writer, "er/note.txt").await.unwrap();
        assert_eq!(text_of(&relative), "h\u{e9}llo\n");
        let elsewhere = read(&reader, "er/note.txt").await.unwrap_err();
        assert!(
            elsewhere.to_string().contains("does not exist"),
            "{elsewhere}"
        );

        let listing = server
            .files_list(
                serde_json::from_value(
                    serde_json::json!({ "session": writer.as_str(), "path": "er" }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            listing.starts_with("/home/computer/deep/er\n1 entries"),
            "{listing}"
        );

        let make = "printf '\\211PNG\\r\\n\\032\\n' > /tmp/t.png; head -c 100 /dev/urandom >> /tmp/t.png; printf '\\000\\001' > /tmp/t.bin";
        let shell = server
            .run_shell(shell_args(&writer, make, None))
            .await
            .unwrap();
        assert!(text_of(&shell).starts_with("exit code: 0"));
        let image = serde_json::to_value(read(&reader, "/tmp/t.png").await.unwrap()).unwrap();
        assert_eq!(image["content"][1]["type"], "image");
        assert_eq!(image["content"][1]["mimeType"], "image/png");
        let binary = read(&reader, "/tmp/t.bin").await.unwrap_err();
        assert!(binary.to_string().contains("shell tool"), "{binary}");

        server.end(writer.as_str()).await.unwrap();
        server.end(reader.as_str()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn native_vnc_clients_authenticate_on_the_screens_port_and_cannot_resize_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let port_base = crate::computer::free_port_base();
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(port_base),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let size = ScreenSize::parse("1024x768").unwrap();
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(size),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );

        let started = server.start("viewed").await.unwrap();
        server.observe(started.session.as_str()).await.unwrap();
        let link = started.link.expect("the computer has a viewer");
        let key = link
            .strip_prefix(&format!("http://127.0.0.1:{port_base}/#key="))
            .expect("the link points at the published page port")
            .to_owned();
        assert_eq!(key.len(), 8);

        let screen_port = port_base + 1;
        let mut rfb = rfb_connect(screen_port).await;
        assert_eq!(rfb.security_types, [2]);
        assert_ne!(rfb_authenticate(&mut rfb, "wrongpwd").await, 0);

        let mut rfb = rfb_connect(screen_port).await;
        assert_eq!(viewers_on_page(port_base, &key).await, 0);
        assert_eq!(rfb_authenticate(&mut rfb, &key).await, 0);
        assert_eq!(rfb_size(&mut rfb).await, (1024, 768));
        let mut counted = 0;
        for _ in 0..20 {
            counted = viewers_on_page(port_base, &key).await;
            if counted == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(counted, 1, "only the authenticated connection is a viewer");
        let mut name_len = [0u8; 20];
        rfb.stream.read_exact(&mut name_len).await.unwrap();
        let name_len = u32::from_be_bytes(name_len[16..20].try_into().unwrap());
        let mut desktop_name = vec![0u8; name_len as usize];
        rfb.stream.read_exact(&mut desktop_name).await.unwrap();

        let mut resize = vec![2, 0, 0, 1];
        resize.extend_from_slice(&(-308i32).to_be_bytes());
        resize.extend_from_slice(&[251, 0, 2, 128, 1, 224, 1, 0]);
        resize.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 2, 128, 1, 224, 0, 0, 0, 0]);
        resize.extend_from_slice(&[3, 0, 0, 0, 0, 0, 4, 0, 3, 0]);
        rfb.stream.write_all(&resize).await.unwrap();
        let mut update = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(10), rfb.stream.read_exact(&mut update))
            .await
            .expect("the server answers the update request")
            .unwrap();
        assert_eq!(update[0], 0, "a FramebufferUpdate follows");

        let mut again = rfb_connect(screen_port).await;
        assert_eq!(rfb_authenticate(&mut again, &key).await, 0);
        assert_eq!(rfb_size(&mut again).await, (1024, 768));

        let mut closed = tokio::net::TcpStream::connect(("127.0.0.1", port_base + 7))
            .await
            .unwrap();
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), closed.read(&mut byte))
            .await
            .expect("a screen that is not open closes the connection");
        assert!(matches!(read, Ok(0) | Err(_)));
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn a_stopped_computer_moves_to_a_newer_image_and_never_back() {
        use std::io::Write;

        let id = Uuid::new_v4().simple().to_string();
        let (older, newer) = (
            format!("computer-use-test-older:{id}"),
            format!("computer-use-test-newer:{id}"),
        );
        let _tags = Tags(vec![newer.clone(), older.clone()]);
        docker_cli(&["tag", image::DEV_IMAGE, &older]);
        let mut build = std::process::Command::new("docker")
            .args(["build", "--quiet", "-t", &newer, "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("the docker CLI runs");
        write!(
            build.stdin.take().unwrap(),
            "FROM {older}
USER root
RUN echo newer > /newer
USER computer
"
        )
        .unwrap();
        assert!(build.wait().unwrap().success());

        let name = format!("computer-use-test-{id}");
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker: docker.clone(),
            name: name.clone(),
            volume: settings.volume(),
        };
        let server_on = |reference: &str| {
            Server::new(
                settings.clone(),
                Image {
                    reference: reference.to_owned(),
                    pull: false,
                    by_version: false,
                },
                Ok(ScreenSize::default()),
                Ok(ShellTimeouts::default()),
                parse_idle(None),
                Ok(open::Mode::None),
            )
        };
        let container_id = || docker_cli(&["inspect", "--format", "{{.Id}}", &name]);

        let old_server = server_on(&older);
        let first = old_server.start("first").await.unwrap();
        let first_id = container_id();
        let first_link = first.link.expect("the computer has a viewer link");
        docker_cli(&[
            "exec",
            &name,
            "sh",
            "-c",
            "echo kept > /home/computer/upgrade-check.txt",
        ]);

        let new_server = server_on(&newer);
        new_server.start("while running").await.unwrap();
        assert_eq!(
            container_id(),
            first_id,
            "a running computer is not touched"
        );
        old_server.shutdown().await;
        new_server.shutdown().await;

        docker_cli(&["stop", "--time", "1", &name]);
        let upgraded = server_on(&newer).start("after stop").await.unwrap();
        let upgraded_id = container_id();
        assert_ne!(upgraded_id, first_id, "the stopped computer is recreated");
        assert_eq!(
            docker_cli(&["inspect", "--format", "{{.Config.Image}}", &name]),
            newer
        );
        assert_eq!(
            docker_cli(&["exec", &name, "cat", "/home/computer/upgrade-check.txt"]),
            "kept"
        );
        assert_eq!(
            upgraded.link.as_deref(),
            Some(first_link.as_str()),
            "the port base and the viewer key survive"
        );

        docker_cli(&["stop", "--time", "1", &name]);
        server_on(&older).start("old binary").await.unwrap();
        assert_eq!(
            container_id(),
            upgraded_id,
            "an older image never replaces a newer one"
        );
    }

    struct HostDir(std::path::PathBuf);

    impl HostDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("computer-use-xfer-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }

    impl Drop for HostDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    #[expect(
        clippy::too_many_lines,
        reason = "one scenario on one computer, so the container starts once"
    )]
    async fn file_transfer_round_trips_a_folder_and_keeps_existing_files_unless_told_to_overwrite()
    {
        let name = format!("computer-use-test-{}", Uuid::new_v4().simple());
        let settings = Settings {
            name: name.clone(),
            timezone: None,
            port_base: Ok(crate::computer::free_port_base()),
        };
        let docker = crate::docker_host::connect().unwrap();
        let _cleanup = Cleanup {
            docker,
            name,
            volume: settings.volume(),
        };
        let server = Server::new(
            settings,
            image::from_env(),
            Ok(ScreenSize::default()),
            Ok(ShellTimeouts::default()),
            parse_idle(None),
            Ok(open::Mode::None),
        );
        let session = server.start("file transfer").await.unwrap().session;
        let transfer =
            |direction: &str, host: &std::path::Path, computer: &str, overwrite: bool| {
                let args: FileTransferArgs = serde_json::from_value(serde_json::json!({
                    "session": session.as_str(),
                    "direction": direction,
                    "host_path": host.to_str().unwrap(),
                    "computer_path": computer,
                    "overwrite": overwrite,
                }))
                .unwrap();
                server.files_transfer(args)
            };
        let shell = |command: &'static str| {
            let args = shell_args(&session, command, None);
            async { server.run_shell(args).await.map(|result| text_of(&result)) }
        };

        let host = HostDir::new();
        let source = host.0.join("xfer");
        std::fs::create_dir_all(source.join("empty")).unwrap();
        std::fs::create_dir_all(source.join("sub")).unwrap();
        let binary: Vec<u8> = (0..=255u8).cycle().take(1_000_003).collect();
        std::fs::write(source.join("data.bin"), &binary).unwrap();
        std::fs::write(source.join("sub/note.txt"), "héllo\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(source.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
            std::fs::set_permissions(
                source.join("run.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::os::unix::fs::symlink("data.bin", source.join("link")).unwrap();
        }

        let reply = transfer("to_computer", &source, "~/xfer", false)
            .await
            .unwrap();
        assert!(
            reply.starts_with("copied to /home/computer/xfer\n"),
            "{reply}"
        );
        let listing = shell("cd ~/xfer && sha256sum data.bin && cat sub/note.txt && ls -d empty && stat -c '%U %a' data.bin")
            .await
            .unwrap();
        let expected = {
            use std::fmt::Write as _;
            let digest = sha256_hex(&binary);
            let mut text = String::new();
            let _ = write!(text, "{digest}  data.bin\nhéllo\nempty\ncomputer 644\n");
            text
        };
        assert!(listing.contains(&expected), "{listing}");

        shell("cd ~/xfer && printf '#!/bin/sh\\n' > tool.sh && chmod 750 tool.sh && ln -s sub/note.txt notelink && mkfifo pipe")
            .await
            .unwrap();
        let again = transfer("to_computer", &source, "~", false)
            .await
            .unwrap_err();
        assert!(
            again.to_string().contains("/home/computer/xfer")
                && again.to_string().contains("overwrite"),
            "{again}"
        );
        let merged = transfer("to_computer", &source, "~", true).await.unwrap();
        assert!(merged.contains("copied to /home/computer/xfer"), "{merged}");
        let kept = shell("ls ~/xfer && find ~/xfer -name '.computer-use-transfer-*'")
            .await
            .unwrap();
        assert!(
            kept.contains("tool.sh") && kept.contains("notelink"),
            "{kept}"
        );
        assert!(!kept.contains("computer-use-transfer"), "{kept}");

        let file_over_folder =
            transfer("to_computer", &source.join("data.bin"), "~/xfer/sub", true)
                .await
                .unwrap();
        assert!(
            file_over_folder.contains("/home/computer/xfer/sub/data.bin"),
            "{file_over_folder}"
        );
        let folder_over_file = transfer("to_computer", &source, "~/xfer/data.bin", true)
            .await
            .unwrap_err();
        assert!(
            folder_over_file.to_string().contains("not a folder"),
            "{folder_over_file}"
        );

        let back = host.0.join("back");
        std::fs::create_dir(&back).unwrap();
        let reply = transfer("from_computer", &back, "~/xfer", false)
            .await
            .unwrap();
        let landed = back.join("xfer");
        assert!(
            reply.starts_with(&format!("copied to {}", landed.display())),
            "{reply}"
        );
        assert_eq!(std::fs::read(landed.join("data.bin")).unwrap(), binary);
        assert_eq!(
            std::fs::read_to_string(landed.join("sub/note.txt")).unwrap(),
            "héllo\n"
        );
        assert!(landed.join("empty").is_dir());
        assert!(
            reply.contains("pipe: it is a pipe, device, or socket"),
            "{reply}"
        );
        assert!(!landed.join("pipe").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |name: &str| {
                std::fs::metadata(landed.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("tool.sh"), 0o750);
            assert_eq!(mode("run.sh"), 0o755);
            assert_eq!(
                std::fs::read_link(landed.join("notelink")).unwrap(),
                std::path::Path::new("sub/note.txt")
            );
        }

        let refused = transfer("from_computer", &back, "~/xfer", false)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("already exists"), "{refused}");
        let one = host.0.join("one.bin");
        transfer("from_computer", &one, "~/xfer/data.bin", false)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&one).unwrap(), binary);
        let missing = transfer("from_computer", &back, "~/nope", false)
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("does not exist"), "{missing}");
        let no_host = transfer("to_computer", &host.0.join("nope"), "~/x", false)
            .await
            .unwrap_err();
        assert!(no_host.to_string().contains("nope"), "{no_host}");

        server.end(session.as_str()).await.unwrap();
        server.shutdown().await;
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        Sha256::digest(bytes)
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}
