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

use crate::image::Image;

pub const NAME_ENV: &str = "COMPUTER_USE_NAME";
pub const PORT_BASE_ENV: &str = "COMPUTER_USE_PORT_BASE";
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

/// Reads `COMPUTER_USE_PORT_BASE`, which must leave room for the viewer page and every screen's VNC port.
pub fn parse_port_base(value: Option<&str>) -> Result<u16, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_PORT_BASE);
    };
    let last = u32::from(SCREEN_COUNT);
    match value.parse::<u16>() {
        Ok(base) if base >= 1024 && u32::from(base) + last <= 65535 => Ok(base),
        _ => Err(format!(
            "{PORT_BASE_ENV} must be a port from 1024 to {}, got `{value}`",
            65535 - last
        )),
    }
}

impl Settings {
    pub fn from_env() -> Self {
        let name = std::env::var(NAME_ENV)
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| DEFAULT_NAME.to_owned());
        let timezone = iana_time_zone::get_timezone()
            .inspect_err(|error| warn!(%error, "could not read the host timezone"))
            .ok();
        let port_base = parse_port_base(std::env::var(PORT_BASE_ENV).ok().as_deref());
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
    Create,
    Start,
    Reuse,
    Refuse(&'static str),
}

pub fn plan(status: Option<ContainerStateStatusEnum>) -> Plan {
    use ContainerStateStatusEnum::{
        CREATED, DEAD, EXITED, PAUSED, REMOVING, RESTARTING, RUNNING, STOPPING,
    };
    match status {
        None => Plan::Create,
        Some(CREATED | EXITED) => Plan::Start,
        Some(RUNNING | RESTARTING) => Plan::Reuse,
        Some(PAUSED) => Plan::Refuse("the computer is paused, run `docker unpause` on it"),
        Some(REMOVING | DEAD | STOPPING) => Plan::Refuse(
            "the computer is stopping, being removed, or dead, wait or remove it and retry",
        ),
        Some(ContainerStateStatusEnum::EMPTY) => {
            Plan::Refuse("the computer is in an unknown state")
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

/// Reads the published `computerd` port and the token from an inspected container.
pub fn endpoint_from(inspect: &ContainerInspectResponse) -> Result<Endpoint> {
    let token = inspect
        .config
        .as_ref()
        .and_then(|config| config.env.as_ref())
        .into_iter()
        .flatten()
        .find_map(|var| var.strip_prefix(&format!("{TOKEN_ENV}=")))
        .filter(|token| !token.is_empty())
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

fn explain_start_error(error: anyhow::Error, creating: bool) -> anyhow::Error {
    match port_conflict(&format!("{error:#}"), creating) {
        Some(message) => anyhow!(message),
        None => error,
    }
}

/// A free host port block of 17 ports for tests that create computers, so parallel tests never collide.
#[cfg(test)]
pub fn free_port_base() -> u16 {
    use std::net::TcpListener;
    let start = 30000 + (Uuid::new_v4().as_u128() % 1500) as u16 * 20;
    (0..1500)
        .map(|step| 30000 + (start - 30000 + step * 20) % 30000)
        .find(|base| {
            (0..=SCREEN_COUNT)
                .all(|offset| TcpListener::bind(("127.0.0.1", base + u16::from(offset))).is_ok())
        })
        .expect("a free block of ports exists")
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

/// Docker side of the computer: creates it, starts it, and finds its endpoint.
pub struct Docked {
    docker: Docker,
    settings: Settings,
    image: Image,
}

impl Docked {
    pub fn connect(settings: Settings, image: Image) -> Result<Self> {
        let docker = Docker::connect_with_defaults()
            .context("connecting to Docker, is Docker running (see DOCKER_HOST)?")?;
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
    pub async fn ensure_running(&self) -> Result<Endpoint> {
        let name = &self.settings.name;
        let found = self.inspect().await?;
        match plan(found.as_ref().and_then(status_of)) {
            Plan::Refuse(reason) => bail!("{reason} (container `{name}`)"),
            Plan::Reuse => {}
            Plan::Start => {
                info!(container = %name, "starting the computer");
                self.start_container()
                    .await
                    .map_err(|error| explain_start_error(error, false))?;
            }
            Plan::Create => self.create().await?,
        }
        self.wait_for_endpoint().await
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

    async fn create(&self) -> Result<()> {
        let name = &self.settings.name;
        let port_base = self
            .settings
            .port_base
            .clone()
            .map_err(|message| anyhow!(message))?;
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
        match created {
            Err(error) if is_conflict(&error) => {
                info!(container = %name, "another process is creating the computer");
                self.wait_until_exists().await?;
            }
            other => {
                other?;
            }
        }
        let Err(error) = self.start_container().await else {
            return Ok(());
        };
        if port_conflict(&format!("{error:#}"), true).is_none() {
            return Err(error);
        }
        let explained = explain_start_error(error, true);
        let remove = RemoveContainerOptionsBuilder::new().force(true).build();
        let _ = within(
            "removing the computer that could not start",
            DOCKER_TIMEOUT,
            self.docker.remove_container(name, Some(remove)),
        )
        .await;
        Err(explained)
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
    fn plan_never_stops_or_recreates_an_existing_computer() {
        use ContainerStateStatusEnum::{CREATED, EXITED, PAUSED, RESTARTING, RUNNING};
        assert_eq!(plan(None), Plan::Create);
        assert_eq!(plan(Some(EXITED)), Plan::Start);
        assert_eq!(plan(Some(CREATED)), Plan::Start);
        assert_eq!(plan(Some(RUNNING)), Plan::Reuse);
        assert_eq!(plan(Some(RESTARTING)), Plan::Reuse);
        assert!(matches!(plan(Some(PAUSED)), Plan::Refuse(_)));
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
    fn the_port_base_leaves_room_for_every_screen() {
        assert_eq!(parse_port_base(None), Ok(20900));
        assert_eq!(parse_port_base(Some(" 21900 ")), Ok(21900));
        assert_eq!(parse_port_base(Some("65519")), Ok(65519));
        for bad in ["65520", "1023", "abc", "-1", "70000"] {
            assert!(parse_port_base(Some(bad)).is_err(), "{bad}");
        }
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
