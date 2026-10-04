use std::{collections::HashMap, future::Future, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use bollard::{
    Docker,
    models::{
        ContainerCreateBody, ContainerInspectResponse, ContainerStateStatusEnum, HostConfig,
        PortBinding, VolumeCreateRequest,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
    },
};
use computer_protocol::{
    API_PORT, DEFAULT_PORT_BASE, HOST_PORT_BASE_ENV, SCREEN_COUNT, TOKEN_ENV, VIEWER_PORT,
};
use futures_util::StreamExt;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    image::Image,
    settings::{self, PORT_BASE_ENV},
    upgrade::{self, Compare, ImageFacts},
};

const DEFAULT_NAME: &str = "computer-use";
const VOLUME_SUFFIX: &str = "-home";
const HOME_DIR: &str = "/home/computer";
const PROJECT_LABEL: &str = "computer-use-mcp";
const SHM_BYTES: i64 = 2 * 1024 * 1024 * 1024;
const PIDS_LIMIT: i64 = 4096;
const LOOPBACK: &str = "127.0.0.1";
const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);
const ENDPOINT_RETRY: Duration = Duration::from_millis(250);
const START_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const PULL_TIMEOUT: Duration = Duration::from_mins(15);

/// Names and host facts used when the computer is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub name: String,
    pub timezone: Option<String>,
    /// Host port of the viewer page. Raw VNC for screen N is on this plus N. An invalid
    /// setting stays a message, so only creating the computer fails.
    pub port_base: Result<u16, String>,
}

impl Settings {
    pub fn from_env() -> Self {
        let name = settings::name().unwrap_or_else(|| DEFAULT_NAME.to_owned());
        let timezone = iana_time_zone::get_timezone()
            .inspect_err(|error| warn!(%error, "could not read the host timezone"))
            .ok();
        let port_base = settings::port_base();
        Self {
            name,
            timezone,
            port_base,
        }
    }

    pub fn volume(&self) -> String {
        format!("{}{VOLUME_SUFFIX}", self.name)
    }
}

/// Where and how to reach `computerd` on a running computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    /// Host port of the viewer page, `None` for a computer made before the viewer existed.
    pub viewer_port: Option<u16>,
}

/// What to do with the container that `start_computer` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Do(Step),
    /// Remove the exited container and create it again on the wanted image.
    Recreate,
}

/// A way to get a usable container that removes nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Create,
    Start,
    Reuse,
    Refuse(&'static str),
}

/// Picks what to do from the container's state, and whether the wanted image is newer than the
/// one the container was made from. Only an exited container is ever recreated. A container that
/// is only `created` may be about to be started by another process.
pub fn plan(status: Option<ContainerStateStatusEnum>, newer_image: bool) -> Plan {
    if newer_image && is_exited(status) {
        Plan::Recreate
    } else {
        Plan::Do(step(status))
    }
}

pub fn step(status: Option<ContainerStateStatusEnum>) -> Step {
    use ContainerStateStatusEnum::{
        CREATED, DEAD, EXITED, PAUSED, REMOVING, RESTARTING, RUNNING, STOPPING,
    };
    match status {
        None => Step::Create,
        Some(CREATED | EXITED) => Step::Start,
        Some(RUNNING | RESTARTING) => Step::Reuse,
        Some(PAUSED) => Step::Refuse("the computer is paused, run `docker unpause` on it"),
        Some(REMOVING | DEAD | STOPPING) => Step::Refuse(
            "the computer is stopping, being removed, or dead, wait or remove it and retry",
        ),
        Some(ContainerStateStatusEnum::EMPTY) => {
            Step::Refuse("the computer is in an unknown state")
        }
    }
}

fn loopback_binding(host_port: String) -> Vec<PortBinding> {
    vec![PortBinding {
        host_ip: Some(LOOPBACK.to_owned()),
        host_port: Some(host_port),
    }]
}

/// Port bindings of the computer: `computerd`'s API on a free host port, and the viewer page and
/// the raw VNC ports at `base` and above.
fn port_bindings(base: u16) -> HashMap<String, Option<Vec<PortBinding>>> {
    let viewer = (0..=SCREEN_COUNT).map(|screen| {
        (
            format!("{}/tcp", VIEWER_PORT + u16::from(screen)),
            Some(loopback_binding((base + u16::from(screen)).to_string())),
        )
    });
    viewer
        .chain([(
            format!("{API_PORT}/tcp"),
            Some(loopback_binding(String::new())),
        )])
        .collect()
}

pub fn container_body(
    settings: &Settings,
    port_base: u16,
    image: &str,
    token: &str,
) -> ContainerCreateBody {
    let mut env = vec![
        format!("{TOKEN_ENV}={token}"),
        format!("{HOST_PORT_BASE_ENV}={port_base}"),
    ];
    env.extend(settings.timezone.iter().map(|tz| format!("TZ={tz}")));
    let bindings = port_bindings(port_base);
    ContainerCreateBody {
        image: Some(image.to_owned()),
        env: Some(env),
        labels: Some(labels()),
        exposed_ports: Some(bindings.keys().cloned().collect()),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{}:{HOME_DIR}", settings.volume())]),
            port_bindings: Some(bindings),
            shm_size: Some(SHM_BYTES),
            pids_limit: Some(PIDS_LIMIT),
            init: Some(true),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    }
}

fn labels() -> HashMap<String, String> {
    HashMap::from([(PROJECT_LABEL.to_owned(), "true".to_owned())])
}

fn endpoint_token(inspect: &ContainerInspectResponse) -> Option<&str> {
    inspect
        .config
        .as_ref()
        .and_then(|config| config.env.as_ref())
        .into_iter()
        .flatten()
        .find_map(|var| var.strip_prefix(&format!("{TOKEN_ENV}=")))
        .filter(|token| !token.is_empty())
}

/// Reads the published `computerd` port and the token from an inspected container.
pub fn endpoint_from(inspect: &ContainerInspectResponse) -> Result<Endpoint> {
    let token = endpoint_token(inspect)
        .ok_or_else(|| anyhow!("the container has no {TOKEN_ENV}, it was not made by this tool"))?
        .to_owned();
    let published = |container: u16| -> Option<u16> {
        inspect
            .network_settings
            .as_ref()
            .and_then(|settings| settings.ports.as_ref())
            .and_then(|ports| ports.get(&format!("{container}/tcp")))
            .and_then(Option::as_ref)
            .into_iter()
            .flatten()
            .find_map(|binding| binding.host_port.as_deref()?.parse().ok())
    };
    let port = published(API_PORT)
        .ok_or_else(|| anyhow!("the container does not publish port {API_PORT} yet"))?;
    Ok(Endpoint {
        port,
        token,
        viewer_port: published(VIEWER_PORT),
    })
}

/// Rewrites Docker's refusal to publish a taken port into a message that says what to change.
///
/// `creating` is true for a computer being made now, which can move to another base. A computer that
/// already exists keeps its ports.
pub fn port_conflict(docker_error: &str, creating: bool) -> Option<String> {
    let taken = docker_error.contains("port is already allocated")
        || docker_error.contains("address already in use")
        || docker_error.contains("ports are not available");
    if !taken {
        return None;
    }
    let digits: String = docker_error
        .split("127.0.0.1:")
        .nth(1)
        .unwrap_or_default()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let port = if digits.is_empty() {
        "one of the computer's ports".to_owned()
    } else {
        format!("port {digits}")
    };
    Some(if creating {
        format!(
            "{port} on 127.0.0.1 is already in use. The computer needs {} ports from {PORT_BASE_ENV} up (default {DEFAULT_PORT_BASE}). Free the port or set {PORT_BASE_ENV} to another base, then call start_computer again",
            u16::from(SCREEN_COUNT) + 1
        )
    } else {
        format!(
            "{port} on 127.0.0.1 is already in use, and this computer was created with those ports. Free the port, or remove the computer with `docker rm` (home stays) so it is created again with another {PORT_BASE_ENV}"
        )
    })
}

/// A start that failed because Docker could not publish a port.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct PortConflict(String);

/// Turns Docker's refusal to publish a taken port (a server error) into a [`PortConflict`] message.
fn explain_start_error(error: anyhow::Error, creating: bool) -> anyhow::Error {
    if !has_status(&error, 500) {
        return error;
    }
    match port_conflict(&format!("{error:#}"), creating) {
        Some(message) => anyhow::Error::new(PortConflict(message)),
        None => error,
    }
}

/// A free host port block of 17 ports for tests that create computers.
///
/// Blocks sit below the Windows ephemeral range (49152 and up), where outgoing connections take
/// ports. Each block is handed out once per process, so parallel tests never collide.
#[cfg(test)]
pub fn free_port_base() -> u16 {
    use std::{collections::HashSet, net::TcpListener, sync::Mutex};
    const FIRST: u16 = 30000;
    const BLOCK: u16 = 17;
    const BLOCKS: u16 = (48000 - FIRST) / BLOCK;
    static TAKEN: Mutex<Option<HashSet<u16>>> = Mutex::new(None);

    let mut taken = TAKEN.lock().expect("the port block lock is held briefly");
    let taken = taken.get_or_insert_with(HashSet::new);
    let start = u16::try_from(Uuid::new_v4().as_u128() % u128::from(BLOCKS))
        .expect("the remainder is below the block count");
    (0..BLOCKS)
        .map(|step| FIRST + (start + step) % BLOCKS * BLOCK)
        .find(|base| {
            !taken.contains(base)
                && (0..BLOCK).all(|offset| TcpListener::bind(("127.0.0.1", base + offset)).is_ok())
        })
        .inspect(|base| {
            taken.insert(*base);
        })
        .expect("a free block of ports exists")
}

fn is_exited(status: Option<ContainerStateStatusEnum>) -> bool {
    status == Some(ContainerStateStatusEnum::EXITED)
}

/// Whether this tool made the container, which is the only kind it may remove.
fn is_ours(inspect: &ContainerInspectResponse) -> bool {
    let labelled = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .is_some_and(|labels| labels.contains_key(PROJECT_LABEL));
    labelled && endpoint_token(inspect).is_some()
}

/// Host port the viewer page was published on when the container was created.
fn configured_port_base(inspect: &ContainerInspectResponse) -> Option<u16> {
    inspect
        .host_config
        .as_ref()?
        .port_bindings
        .as_ref()?
        .get(&format!("{VIEWER_PORT}/tcp"))?
        .as_ref()?
        .iter()
        .find_map(|binding| binding.host_port.as_deref()?.parse().ok())
}

fn status_of(inspect: &ContainerInspectResponse) -> Option<ContainerStateStatusEnum> {
    inspect.state.as_ref().and_then(|state| state.status)
}

async fn within<T, E>(
    what: &str,
    limit: Duration,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(limit, call).await {
        Ok(result) => result.with_context(|| what.to_owned()),
        Err(_) => bail!("{what} timed out after {} s", limit.as_secs()),
    }
}

fn has_status(error: &anyhow::Error, wanted: u16) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<bollard::errors::Error>(),
            Some(bollard::errors::Error::DockerResponseServerError { status_code, .. })
                if *status_code == wanted
        )
    })
}

fn is_not_found(error: &anyhow::Error) -> bool {
    has_status(error, 404)
}

fn is_conflict(error: &anyhow::Error) -> bool {
    has_status(error, 409)
}

/// Why a computer is being created.
enum Origin {
    New,
    /// Another container is being replaced. `port_base` is the one it was published on, if any.
    Replacing {
        port_base: Option<u16>,
    },
}

/// Docker side of the computer: creates it, starts it, and finds its endpoint.
pub struct Docked {
    docker: Docker,
    settings: Settings,
    image: Image,
}

impl Docked {
    pub fn connect(settings: Settings, image: Image) -> Result<Self> {
        let docker = crate::docker_host::connect()?;
        Ok(Self {
            docker,
            settings,
            image,
        })
    }

    pub fn name(&self) -> &str {
        &self.settings.name
    }

    async fn inspect(&self) -> Result<Option<ContainerInspectResponse>> {
        let name = &self.settings.name;
        let result = within(
            "inspecting the computer container",
            DOCKER_TIMEOUT,
            self.docker.inspect_container(name, None),
        )
        .await;
        match result {
            Ok(inspect) => Ok(Some(inspect)),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Finds the endpoint of a running computer without changing anything.
    pub async fn running_endpoint(&self) -> Result<Option<Endpoint>> {
        match self.inspect().await? {
            Some(inspect) if status_of(&inspect) == Some(ContainerStateStatusEnum::RUNNING) => {
                endpoint_from(&inspect).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Creates or starts the computer as needed and returns its endpoint.
    ///
    /// A stopped computer on an older image is removed and created again on the wanted image. A
    /// running one is never touched.
    pub async fn ensure_running(&self) -> Result<Endpoint> {
        let found = self.inspect().await?;
        let status = found.as_ref().and_then(status_of);
        let newer_image = match &found {
            Some(found) if is_exited(status) && is_ours(found) => {
                self.wanted_image_is_newer(found).await
            }
            _ => false,
        };
        match plan(status, newer_image) {
            Plan::Recreate => {
                let old = found.expect("a computer is only recreated when it exists");
                Box::pin(self.recreate(&old)).await?;
            }
            Plan::Do(step) => self.apply(step).await?,
        }
        self.wait_for_endpoint().await
    }

    async fn apply(&self, step: Step) -> Result<()> {
        let name = &self.settings.name;
        match step {
            Step::Refuse(reason) => bail!("{reason} (container `{name}`)"),
            Step::Reuse => {}
            Step::Start => {
                info!(container = %name, "starting the computer");
                self.start_or_join()
                    .await
                    .map_err(|error| explain_start_error(error, false))?;
            }
            Step::Create => self.create(Origin::New).await?,
        }
        Ok(())
    }

    pub fn compare(&self) -> Compare {
        if self.image.by_version {
            Compare::Version
        } else {
            Compare::Created
        }
    }

    async fn image_facts(&self, reference: &str) -> Result<ImageFacts> {
        let inspect = within(
            "looking for the computer image",
            DOCKER_TIMEOUT,
            self.docker.inspect_image(reference),
        )
        .await?;
        ImageFacts::from_inspect(&inspect)
            .ok_or_else(|| anyhow!("Docker returned no ID for image `{reference}`"))
    }

    /// Facts about the image this build wants, `None` when it is not available locally.
    pub async fn wanted_image(&self) -> Result<Option<ImageFacts>> {
        match self.image_facts(&self.image.reference).await {
            Ok(facts) => Ok(Some(facts)),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Facts about the image an existing container was made from.
    pub async fn container_image(&self, found: &ContainerInspectResponse) -> Result<ImageFacts> {
        let id = found
            .image
            .as_deref()
            .ok_or_else(|| anyhow!("the container does not name its image"))?;
        self.image_facts(id).await
    }

    /// Lines for `info` about the computer container, if there is one.
    pub async fn describe(&self) -> Result<Vec<String>> {
        let name = &self.settings.name;
        let Some(found) = self.inspect().await? else {
            return Ok(vec![format!(
                "computer: none yet (container `{name}`), call start_computer to create it"
            )]);
        };
        let status = found.state.as_ref().and_then(|state| state.status);
        let state = status.map_or_else(|| "unknown".to_owned(), |status| status.to_string());
        let mut lines = vec![format!("computer: {state} (container `{name}`)")];
        match configured_port_base(&found) {
            Some(base) => lines.push(format!("port base: {base}")),
            None => lines.push("port base: none, this computer predates the viewer".to_owned()),
        }
        let have = match self.container_image(&found).await {
            Ok(have) => have,
            Err(error) => {
                lines.push(format!("computer image: unavailable ({error:#})"));
                return Ok(lines);
            }
        };
        lines.push(format!(
            "computer image: version {}, image {}",
            have.version.as_deref().unwrap_or("unknown"),
            have.short_id()
        ));
        let upgrade = match self.wanted_image().await {
            Ok(_) if !is_ours(&found) => "none, this container was not made by this tool",
            Ok(Some(wanted)) if upgrade::is_newer(&have, &wanted, self.compare()) => {
                if is_exited(status) {
                    "pending, the computer will be recreated on the next start_computer"
                } else {
                    "pending, it happens on start_computer after the computer is stopped (`docker stop`)"
                }
            }
            Ok(Some(wanted)) if upgrade::is_newer(&wanted, &have, self.compare()) => {
                "none, the computer is on a newer image than this build, update the binary"
            }
            Ok(Some(_)) => "none, the computer is on this build's image",
            Ok(None) => "unknown, this build's image is not available locally",
            Err(_) => "unknown, could not inspect this build's image",
        };
        lines.push(format!("upgrade: {upgrade}"));
        Ok(lines)
    }

    /// Whether the image this build wants is newer than the one the container was made from.
    /// Anything that cannot be found out counts as no, so the computer starts as it is.
    async fn wanted_image_is_newer(&self, found: &ContainerInspectResponse) -> bool {
        let decide = async {
            self.ensure_image().await?;
            let wanted = self.image_facts(&self.image.reference).await?;
            let have = self.container_image(found).await?;
            Ok::<_, anyhow::Error>((
                upgrade::is_newer(&have, &wanted, self.compare()),
                have,
                wanted,
            ))
        };
        match decide.await {
            Ok((newer, have, wanted)) => {
                if newer {
                    info!(
                        container = %self.settings.name,
                        from_version = have.version.as_deref().unwrap_or("unknown"),
                        from_image = have.short_id(),
                        to_version = wanted.version.as_deref().unwrap_or("unknown"),
                        to_image = wanted.short_id(),
                        "the stopped computer is on an older image, recreating it"
                    );
                }
                newer
            }
            Err(error) => {
                warn!(error = %format!("{error:#}"), "could not check for a newer computer image, starting the computer as it is");
                false
            }
        }
    }

    /// Removes the stopped container `old` and creates it again on the wanted image, keeping its
    /// ports. Home is a volume and stays.
    async fn recreate(&self, old: &ContainerInspectResponse) -> Result<()> {
        let name = &self.settings.name;
        let id = old
            .id
            .as_deref()
            .ok_or_else(|| anyhow!("the computer container has no ID"))?;
        let port_base = configured_port_base(old);
        let still_exited = match self.inspect().await? {
            Some(now) => now.id.as_deref() == Some(id) && is_exited(status_of(&now)),
            None => false,
        };
        let removed = still_exited && {
            let options = RemoveContainerOptionsBuilder::new().force(false).build();
            let result = within(
                "removing the old computer container",
                DOCKER_TIMEOUT,
                self.docker.remove_container(id, Some(options)),
            )
            .await;
            match result {
                Ok(()) => true,
                Err(error) if is_not_found(&error) || is_conflict(&error) => false,
                Err(error) => return Err(error),
            }
        };
        if !removed {
            info!(container = %name, "another process changed the computer while upgrading, using its container");
            return match self.inspect().await? {
                Some(now) => self.apply(step(status_of(&now))).await,
                None => self.create(Origin::Replacing { port_base }).await,
            };
        }
        self.create(Origin::Replacing { port_base }).await
    }

    /// Docker reserves the name before the container can be inspected or started.
    async fn wait_until_exists(&self) -> Result<()> {
        let started = tokio::time::Instant::now();
        while self.inspect().await?.is_none() {
            if started.elapsed() >= ENDPOINT_TIMEOUT {
                bail!(
                    "the computer container `{}` was never created",
                    self.settings.name
                );
            }
            tokio::time::sleep(ENDPOINT_RETRY).await;
        }
        Ok(())
    }

    /// Polls until the container runs with its port published, which takes a moment after
    /// a start by this or another process.
    async fn wait_for_endpoint(&self) -> Result<Endpoint> {
        let name = &self.settings.name;
        let started = tokio::time::Instant::now();
        loop {
            let inspect = self
                .inspect()
                .await?
                .ok_or_else(|| anyhow!("the computer container `{name}` disappeared"))?;
            let running = status_of(&inspect) == Some(ContainerStateStatusEnum::RUNNING);
            match endpoint_from(&inspect) {
                Ok(endpoint) if running => return Ok(endpoint),
                Err(error) if started.elapsed() >= ENDPOINT_TIMEOUT => return Err(error),
                _ if started.elapsed() >= ENDPOINT_TIMEOUT => {
                    bail!("the computer container `{name}` is not running")
                }
                _ => tokio::time::sleep(ENDPOINT_RETRY).await,
            }
        }
    }

    /// Creates and starts the computer. A replacement keeps the port base of the container it
    /// replaces, so links to it keep working.
    async fn create(&self, origin: Origin) -> Result<()> {
        let name = &self.settings.name;
        let recreating = matches!(origin, Origin::Replacing { .. });
        let port_base = match origin {
            Origin::Replacing {
                port_base: Some(base),
            } => base,
            Origin::New | Origin::Replacing { port_base: None } => self
                .settings
                .port_base
                .clone()
                .map_err(|message| anyhow!(message))?,
        };
        self.ensure_image().await?;
        let volume = self.settings.volume();
        within(
            "creating the home volume",
            DOCKER_TIMEOUT,
            self.docker.create_volume(VolumeCreateRequest {
                name: Some(volume.clone()),
                labels: Some(labels()),
                ..VolumeCreateRequest::default()
            }),
        )
        .await?;
        let token = Uuid::new_v4().simple().to_string();
        let body = container_body(&self.settings, port_base, &self.image.reference, &token);
        let options = CreateContainerOptionsBuilder::new().name(name).build();
        info!(container = %name, %volume, image = %self.image.reference, "creating the computer");
        let created = within(
            "creating the computer container",
            DOCKER_TIMEOUT,
            self.docker.create_container(Some(options), body),
        )
        .await;
        let made_here = match created {
            Err(error) if is_conflict(&error) => {
                info!(container = %name, "another process is creating the computer");
                self.wait_until_exists().await?;
                false
            }
            other => {
                other?;
                true
            }
        };
        if !made_here && self.wait_until_running().await? {
            info!(container = %name, "another process started the computer");
            return Ok(());
        }
        let Err(error) = self.start_or_join().await else {
            return Ok(());
        };
        let explained = explain_start_error(error, made_here && !recreating);
        if made_here && !recreating && explained.is::<PortConflict>() {
            let remove = RemoveContainerOptionsBuilder::new().force(true).build();
            let _ = within(
                "removing the computer that could not start",
                DOCKER_TIMEOUT,
                self.docker.remove_container(name, Some(remove)),
            )
            .await;
        }
        Err(explained)
    }

    /// Whether our container runs. Docker Desktop for Windows loses the published ports of a
    /// running container when a second start call reaches it, so a container that already runs
    /// must never be started again.
    async fn is_running(&self) -> Result<bool> {
        Ok(self.inspect().await?.is_some_and(|found| {
            is_ours(&found) && status_of(&found) == Some(ContainerStateStatusEnum::RUNNING)
        }))
    }

    /// Polls until the container runs, for up to `START_JOIN_TIMEOUT`.
    async fn wait_until_running(&self) -> Result<bool> {
        let started = tokio::time::Instant::now();
        loop {
            if self.is_running().await? {
                return Ok(true);
            }
            if started.elapsed() >= START_JOIN_TIMEOUT {
                return Ok(false);
            }
            tokio::time::sleep(ENDPOINT_RETRY).await;
        }
    }

    /// Starts the container unless it already runs. When the start is refused with a server error
    /// or a conflict because another process is starting the same container, waits briefly for
    /// that start to finish and treats a container that ends up running as started.
    async fn start_or_join(&self) -> Result<()> {
        if self.is_running().await? {
            return Ok(());
        }
        let Err(error) = self.start_container().await else {
            return Ok(());
        };
        if !(has_status(&error, 500) || is_conflict(&error)) {
            return Err(error);
        }
        if self.wait_until_running().await? {
            info!(container = %self.settings.name, "another process started the computer");
            return Ok(());
        }
        Err(error)
    }

    async fn start_container(&self) -> Result<()> {
        within(
            "starting the computer container",
            DOCKER_TIMEOUT,
            self.docker.start_container(&self.settings.name, None),
        )
        .await
    }

    async fn ensure_image(&self) -> Result<()> {
        let reference = &self.image.reference;
        let present = within(
            "looking for the computer image",
            DOCKER_TIMEOUT,
            self.docker.inspect_image(reference),
        )
        .await;
        match present {
            Ok(_) => return Ok(()),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error),
        }
        if !self.image.pull {
            bail!("image `{reference}` is not available locally, run `just image` to build it");
        }
        info!(image = %reference, "pulling the computer image");
        let options = CreateImageOptionsBuilder::new()
            .from_image(reference)
            .build();
        let pull = async {
            let mut progress = self.docker.create_image(Some(options), None, None);
            while let Some(step) = progress.next().await {
                step?;
            }
            Ok::<(), bollard::errors::Error>(())
        };
        within("pulling the computer image", PULL_TIMEOUT, pull)
            .await
            .with_context(|| format!("pulling `{reference}`, is it public or are you logged in?"))
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{ContainerConfig, NetworkSettings};

    use super::*;

    #[test]
    fn plan_recreates_only_a_stopped_computer_on_an_older_image() {
        use ContainerStateStatusEnum::{CREATED, EXITED, PAUSED, RESTARTING, RUNNING};
        assert_eq!(plan(None, true), Plan::Do(Step::Create));
        assert_eq!(plan(Some(EXITED), false), Plan::Do(Step::Start));
        assert_eq!(plan(Some(EXITED), true), Plan::Recreate);
        assert_eq!(plan(Some(CREATED), true), Plan::Do(Step::Start));
        assert_eq!(plan(Some(RUNNING), true), Plan::Do(Step::Reuse));
        assert_eq!(plan(Some(RESTARTING), true), Plan::Do(Step::Reuse));
        assert!(matches!(
            plan(Some(PAUSED), true),
            Plan::Do(Step::Refuse(_))
        ));
    }

    #[test]
    fn only_a_container_with_our_label_and_token_counts_as_ours() {
        let container =
            |labels: Option<HashMap<String, String>>, env: Vec<&str>| ContainerInspectResponse {
                config: Some(ContainerConfig {
                    labels,
                    env: Some(env.into_iter().map(str::to_owned).collect()),
                    ..ContainerConfig::default()
                }),
                ..ContainerInspectResponse::default()
            };
        assert!(is_ours(&container(
            Some(labels()),
            vec!["COMPUTERD_TOKEN=t"]
        )));
        assert!(!is_ours(&container(None, vec!["COMPUTERD_TOKEN=t"])));
        assert!(!is_ours(&container(Some(labels()), vec!["PATH=/bin"])));
        assert!(!is_ours(&ContainerInspectResponse::default()));
    }

    #[test]
    fn a_recreated_computer_keeps_the_port_base_of_the_old_one() {
        let settings = Settings {
            name: "box".to_owned(),
            timezone: None,
            port_base: Ok(30000),
        };
        let body = container_body(&settings, 31000, "img:1", "tok");
        let old = ContainerInspectResponse {
            host_config: body.host_config,
            ..ContainerInspectResponse::default()
        };
        assert_eq!(configured_port_base(&old), Some(31000));
        assert_eq!(
            configured_port_base(&ContainerInspectResponse::default()),
            None
        );
    }

    #[test]
    fn container_matches_the_documented_settings() {
        let settings = Settings {
            name: "box".to_owned(),
            timezone: Some("America/Sao_Paulo".to_owned()),
            port_base: Ok(21900),
        };
        let body = container_body(&settings, 21900, "img:1", "tok");
        let host = body.host_config.unwrap();
        assert_eq!(settings.volume(), "box-home");
        assert_eq!(host.binds, Some(vec!["box-home:/home/computer".to_owned()]));
        assert_eq!(host.shm_size, Some(2_147_483_648));
        assert_eq!(host.pids_limit, Some(4096));
        assert_eq!(host.init, Some(true));
        assert_eq!(host.memory, None);
        assert_eq!(host.nano_cpus, None);
        assert_eq!(host.cap_add, None);
        let bindings = host.port_bindings.unwrap();
        let host_port = |container: &str| {
            let binding = bindings[container].clone().unwrap();
            assert_eq!(binding.len(), 1);
            assert_eq!(binding[0].host_ip.as_deref(), Some("127.0.0.1"));
            binding[0].host_port.clone().unwrap()
        };
        assert_eq!(host_port("7070/tcp"), "");
        assert_eq!(host_port("20900/tcp"), "21900");
        assert_eq!(host_port("20901/tcp"), "21901");
        assert_eq!(host_port("20916/tcp"), "21916");
        assert_eq!(bindings.len(), 18);
        assert_eq!(
            body.env,
            Some(vec![
                "COMPUTERD_TOKEN=tok".to_owned(),
                "COMPUTERD_HOST_PORT_BASE=21900".to_owned(),
                "TZ=America/Sao_Paulo".to_owned()
            ])
        );
        assert_eq!(body.labels, Some(labels()));
    }

    #[test]
    fn endpoint_is_read_back_from_the_container() {
        let inspect = ContainerInspectResponse {
            config: Some(ContainerConfig {
                env: Some(vec![
                    "PATH=/bin".to_owned(),
                    "COMPUTERD_TOKEN=tok".to_owned(),
                ]),
                ..ContainerConfig::default()
            }),
            network_settings: Some(NetworkSettings {
                ports: Some(HashMap::from([
                    (
                        "7070/tcp".to_owned(),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".to_owned()),
                            host_port: Some("49153".to_owned()),
                        }]),
                    ),
                    (
                        "20900/tcp".to_owned(),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".to_owned()),
                            host_port: Some("21900".to_owned()),
                        }]),
                    ),
                ])),
                ..NetworkSettings::default()
            }),
            ..ContainerInspectResponse::default()
        };
        assert_eq!(
            endpoint_from(&inspect).unwrap(),
            Endpoint {
                port: 49153,
                token: "tok".to_owned(),
                viewer_port: Some(21900),
            }
        );
        assert!(endpoint_from(&ContainerInspectResponse::default()).is_err());
    }

    #[test]
    fn a_taken_port_is_reported_with_the_port_and_the_setting_to_change() {
        let docker = "driver failed programming external connectivity on endpoint x: Bind for 127.0.0.1:20903 failed: port is already allocated";
        let creating = port_conflict(docker, true).unwrap();
        assert!(creating.contains("port 20903"), "{creating}");
        assert!(creating.contains("COMPUTER_USE_PORT_BASE"), "{creating}");
        assert!(creating.contains("17 ports"), "{creating}");
        let existing = port_conflict(docker, false).unwrap();
        assert!(existing.contains("docker rm"), "{existing}");
        assert_eq!(port_conflict("no such image", true), None);
    }
}
