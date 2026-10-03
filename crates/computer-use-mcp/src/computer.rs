use std::{collections::HashMap, future::Future, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use bollard::{
    Docker,
    models::{
        ContainerCreateBody, ContainerInspectResponse, ContainerStateStatusEnum, HostConfig,
        PortBinding, VolumeCreateRequest,
    },
    query_parameters::{CreateContainerOptionsBuilder, CreateImageOptionsBuilder},
};
use computer_protocol::{API_PORT, TOKEN_ENV};
use futures_util::StreamExt;
use tracing::{info, warn};
use uuid::Uuid;

use crate::image::Image;

pub const NAME_ENV: &str = "COMPUTER_USE_NAME";
const DEFAULT_NAME: &str = "computer-use";
const VOLUME_SUFFIX: &str = "-home";
const HOME_DIR: &str = "/home/computer";
const PROJECT_LABEL: &str = "computer-use-mcp";
const SHM_BYTES: i64 = 2 * 1024 * 1024 * 1024;
const PIDS_LIMIT: i64 = 4096;
const LOOPBACK: &str = "127.0.0.1";
const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_TIMEOUT: Duration = Duration::from_mins(15);

/// Names and host facts used when the computer is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub name: String,
    pub timezone: Option<String>,
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
        Self { name, timezone }
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

pub fn container_body(settings: &Settings, image: &str, token: &str) -> ContainerCreateBody {
    let mut env = vec![format!("{TOKEN_ENV}={token}")];
    env.extend(settings.timezone.iter().map(|tz| format!("TZ={tz}")));
    let api_port = format!("{API_PORT}/tcp");
    ContainerCreateBody {
        image: Some(image.to_owned()),
        env: Some(env),
        labels: Some(labels()),
        exposed_ports: Some(vec![api_port.clone()]),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{}:{HOME_DIR}", settings.volume())]),
            port_bindings: Some(HashMap::from([(
                api_port,
                Some(vec![PortBinding {
                    host_ip: Some(LOOPBACK.to_owned()),
                    host_port: Some(String::new()),
                }]),
            )])),
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
    let port = inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.ports.as_ref())
        .and_then(|ports| ports.get(&format!("{API_PORT}/tcp")))
        .and_then(Option::as_ref)
        .into_iter()
        .flatten()
        .find_map(|binding| binding.host_port.as_deref()?.parse().ok())
        .ok_or_else(|| anyhow!("the container does not publish port {API_PORT} yet"))?;
    Ok(Endpoint { port, token })
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

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<bollard::errors::Error>(),
            Some(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        )
    })
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
                within(
                    "starting the computer container",
                    DOCKER_TIMEOUT,
                    self.docker.start_container(name, None),
                )
                .await?;
            }
            Plan::Create => self.create().await?,
        }
        let inspect = self
            .inspect()
            .await?
            .ok_or_else(|| anyhow!("the computer container `{name}` disappeared"))?;
        endpoint_from(&inspect)
    }

    async fn create(&self) -> Result<()> {
        let name = &self.settings.name;
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
        let body = container_body(&self.settings, &self.image.reference, &token);
        let options = CreateContainerOptionsBuilder::new().name(name).build();
        info!(container = %name, %volume, image = %self.image.reference, "creating the computer");
        within(
            "creating the computer container",
            DOCKER_TIMEOUT,
            self.docker.create_container(Some(options), body),
        )
        .await?;
        within(
            "starting the computer container",
            DOCKER_TIMEOUT,
            self.docker.start_container(name, None),
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
        };
        let body = container_body(&settings, "img:1", "tok");
        let host = body.host_config.unwrap();
        assert_eq!(settings.volume(), "box-home");
        assert_eq!(host.binds, Some(vec!["box-home:/home/computer".to_owned()]));
        assert_eq!(host.shm_size, Some(2_147_483_648));
        assert_eq!(host.pids_limit, Some(4096));
        assert_eq!(host.init, Some(true));
        assert_eq!(host.memory, None);
        assert_eq!(host.nano_cpus, None);
        assert_eq!(host.cap_add, None);
        assert_eq!(
            host.port_bindings.unwrap()["7070/tcp"],
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_owned()),
                host_port: Some(String::new()),
            }])
        );
        assert_eq!(
            body.env,
            Some(vec![
                "COMPUTERD_TOKEN=tok".to_owned(),
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
                ports: Some(HashMap::from([(
                    "7070/tcp".to_owned(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_owned()),
                        host_port: Some("49153".to_owned()),
                    }]),
                )])),
                ..NetworkSettings::default()
            }),
            ..ContainerInspectResponse::default()
        };
        assert_eq!(
            endpoint_from(&inspect).unwrap(),
            Endpoint {
                port: 49153,
                token: "tok".to_owned()
            }
        );
        assert!(endpoint_from(&ContainerInspectResponse::default()).is_err());
    }
}
