//! Picks the Docker endpoint the way the Docker CLI does, so `docker context use` is honored.
//! Order: `DOCKER_HOST`, `DOCKER_CONTEXT`, `currentContext` in the CLI config, platform default.

// Outside constraint: bollard 0.21 reads only `DOCKER_HOST`. Drop this module once bollard
// resolves contexts in the same order.

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

const HOST_ENV: &str = "DOCKER_HOST";
const CONTEXT_ENV: &str = "DOCKER_CONTEXT";
const CONFIG_ENV: &str = "DOCKER_CONFIG";
const DEFAULT_CONTEXT: &str = "default";
const WINDOWS_DEFAULT_HOST: &str = "npipe:////./pipe/docker_engine";
const UNIX_DEFAULT_HOST: &str = "unix:///var/run/docker.sock";
const SUPPORTED_SCHEMES: [&str; 4] = ["unix://", "npipe://", "tcp://", "http://"];

/// Why an endpoint was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    DockerHost,
    DockerContext(String),
    CurrentContext(String),
    PlatformDefault,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DockerHost => write!(f, "{HOST_ENV} is set"),
            Self::DockerContext(name) => write!(f, "{CONTEXT_ENV} is `{name}`"),
            Self::CurrentContext(name) => write!(f, "Docker CLI current context is `{name}`"),
            Self::PlatformDefault => write!(f, "platform default"),
        }
    }
}

/// A Docker context's endpoint, read from its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextEndpoint {
    host: String,
    /// The context carries TLS files, which this build cannot use.
    uses_tls: bool,
}

/// The chosen endpoint and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub host: String,
    pub reason: Reason,
}

impl fmt::Display for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.host, self.reason)
    }
}

/// Everything the choice depends on, already read from the environment and disk.
struct Inputs<'a> {
    docker_host: Option<&'a str>,
    docker_context: Option<&'a str>,
    current_context: Option<&'a str>,
    windows: bool,
}

/// Chooses the endpoint. `lookup` returns the endpoint of a named context, or `None` when no such
/// context exists.
fn resolve(
    inputs: &Inputs<'_>,
    lookup: impl Fn(&str) -> Result<Option<ContextEndpoint>>,
) -> Result<Resolved> {
    let resolved = if let Some(host) = inputs.docker_host {
        Resolved {
            host: host.to_owned(),
            reason: Reason::DockerHost,
        }
    } else {
        // An explicit `DOCKER_CONTEXT` replaces `currentContext`, even when it names `default`.
        let (name, reason): (Option<&str>, fn(String) -> Reason) = match inputs.docker_context {
            Some(name) => (Some(name), Reason::DockerContext),
            None => (inputs.current_context, Reason::CurrentContext),
        };
        match name.filter(|name| *name != DEFAULT_CONTEXT) {
            Some(name) => from_context(name, reason(name.to_owned()), &lookup)?,
            None => Resolved {
                host: if inputs.windows {
                    WINDOWS_DEFAULT_HOST
                } else {
                    UNIX_DEFAULT_HOST
                }
                .to_owned(),
                reason: Reason::PlatformDefault,
            },
        }
    };
    check_scheme(&resolved)?;
    Ok(resolved)
}

fn from_context(
    name: &str,
    reason: Reason,
    lookup: &impl Fn(&str) -> Result<Option<ContextEndpoint>>,
) -> Result<Resolved> {
    let endpoint = lookup(name)?.ok_or_else(|| {
        anyhow!(
            "Docker context `{name}` does not exist, run `docker context ls` to list contexts or `docker context use default`"
        )
    })?;
    if endpoint.uses_tls {
        bail!(
            "Docker context `{name}` needs TLS, which this server does not support, use a context with a unix socket or named pipe"
        );
    }
    Ok(Resolved {
        host: endpoint.host,
        reason,
    })
}

fn check_scheme(resolved: &Resolved) -> Result<()> {
    if SUPPORTED_SCHEMES
        .iter()
        .any(|scheme| resolved.host.starts_with(scheme))
    {
        return Ok(());
    }
    bail!(
        "Docker endpoint `{}` ({}) is not supported, use unix://, npipe://, tcp:// or http://",
        resolved.host,
        resolved.reason
    )
}

/// The context folder name the Docker CLI uses: the hex SHA-256 of the context name.
fn context_dir_name(name: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(name.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "currentContext")]
    current_context: Option<String>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "Endpoints", default)]
    endpoints: HashMap<String, MetaEndpoint>,
}

#[derive(Deserialize)]
struct MetaEndpoint {
    #[serde(rename = "Host", default)]
    host: String,
}

/// Reads `currentContext` from the config file text. Unparsable text means no context.
fn parse_current_context(text: &str) -> Option<String> {
    serde_json::from_str::<Config>(text)
        .ok()?
        .current_context
        .filter(|name| !name.is_empty())
}

fn parse_context_endpoint(text: &str, uses_tls: bool) -> Result<ContextEndpoint> {
    let meta: Meta = serde_json::from_str(text).context("parsing the context metadata")?;
    let docker = meta
        .endpoints
        .get("docker")
        .filter(|endpoint| !endpoint.host.is_empty())
        .ok_or_else(|| anyhow!("the context has no Docker endpoint"))?;
    Ok(ContextEndpoint {
        host: docker.host.clone(),
        uses_tls,
    })
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = non_empty_env(CONFIG_ENV) {
        return Some(PathBuf::from(dir));
    }
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    non_empty_env(home).map(|home| PathBuf::from(home).join(".docker"))
}

fn read_context(config_dir: &Path, name: &str) -> Result<Option<ContextEndpoint>> {
    let dir_name = context_dir_name(name);
    let meta_path = config_dir
        .join("contexts")
        .join("meta")
        .join(&dir_name)
        .join("meta.json");
    let text = match std::fs::read_to_string(&meta_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", meta_path.display()));
        }
    };
    let uses_tls = config_dir
        .join("contexts")
        .join("tls")
        .join(&dir_name)
        .join("docker")
        .is_dir();
    parse_context_endpoint(&text, uses_tls)
        .with_context(|| format!("reading Docker context `{name}`"))
        .map(Some)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// Chooses the endpoint from the environment and the Docker CLI config files.
///
/// # Errors
/// Fails when a chosen context is unknown or unusable.
pub fn from_env() -> Result<Resolved> {
    let config_dir = config_dir();
    let current_context = config_dir
        .as_ref()
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|text| parse_current_context(&text));
    let docker_host = non_empty_env(HOST_ENV);
    let docker_context = non_empty_env(CONTEXT_ENV);
    resolve(
        &Inputs {
            docker_host: docker_host.as_deref(),
            docker_context: docker_context.as_deref(),
            current_context: current_context.as_deref(),
            windows: cfg!(windows),
        },
        |name| match &config_dir {
            Some(dir) => read_context(dir, name),
            None => Ok(None),
        },
    )
}

/// Connects to the chosen Docker endpoint. Nothing is sent until the first call.
///
/// # Errors
/// Names the endpoint and why it was chosen when it cannot be used.
pub fn connect() -> Result<Docker> {
    let resolved = from_env().context("choosing the Docker endpoint")?;
    Docker::connect_with_host(&resolved.host)
        .with_context(|| format!("connecting to Docker at {resolved}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(host: &str) -> ContextEndpoint {
        ContextEndpoint {
            host: host.to_owned(),
            uses_tls: false,
        }
    }

    fn lookup(name: &str) -> Result<Option<ContextEndpoint>> {
        if name == "broken" {
            bail!("meta.json is unreadable");
        }
        Ok(match name {
            "orbstack" => Some(context("unix:///Users/me/.orbstack/run/docker.sock")),
            "colima" => Some(context("unix:///Users/me/.colima/default/docker.sock")),
            "desktop-linux" => Some(context("npipe:////./pipe/dockerDesktopLinuxEngine")),
            "remote" => Some(ContextEndpoint {
                host: "tcp://10.0.0.5:2376".to_owned(),
                uses_tls: true,
            }),
            "weird" => Some(context("ssh://me@box")),
            _ => None,
        })
    }

    fn inputs<'a>(
        docker_host: Option<&'a str>,
        docker_context: Option<&'a str>,
        current_context: Option<&'a str>,
    ) -> Inputs<'a> {
        Inputs {
            docker_host,
            docker_context,
            current_context,
            windows: false,
        }
    }

    #[test]
    fn docker_host_beats_every_context() {
        let resolved = resolve(
            &inputs(Some("tcp://1.2.3.4:2375"), Some("colima"), Some("orbstack")),
            lookup,
        )
        .unwrap();
        assert_eq!(
            resolved,
            Resolved {
                host: "tcp://1.2.3.4:2375".to_owned(),
                reason: Reason::DockerHost
            }
        );
    }

    #[test]
    fn docker_context_beats_current_context() {
        let resolved = resolve(&inputs(None, Some("colima"), Some("orbstack")), lookup).unwrap();
        assert_eq!(
            resolved,
            Resolved {
                host: "unix:///Users/me/.colima/default/docker.sock".to_owned(),
                reason: Reason::DockerContext("colima".to_owned())
            }
        );
    }

    #[test]
    fn current_context_is_used_without_overrides() {
        let resolved = resolve(&inputs(None, None, Some("orbstack")), lookup).unwrap();
        assert_eq!(
            resolved,
            Resolved {
                host: "unix:///Users/me/.orbstack/run/docker.sock".to_owned(),
                reason: Reason::CurrentContext("orbstack".to_owned())
            }
        );
    }

    #[test]
    fn named_pipe_context_is_accepted() {
        let resolved = resolve(&inputs(None, None, Some("desktop-linux")), lookup).unwrap();
        assert_eq!(resolved.host, "npipe:////./pipe/dockerDesktopLinuxEngine");
    }

    #[test]
    fn default_context_means_platform_default() {
        let unix = resolve(&inputs(None, None, Some("default")), lookup).unwrap();
        assert_eq!(unix.host, "unix:///var/run/docker.sock");
        assert_eq!(unix.reason, Reason::PlatformDefault);

        let windows = resolve(
            &Inputs {
                windows: true,
                ..inputs(None, Some("default"), Some("orbstack"))
            },
            lookup,
        )
        .unwrap();
        assert_eq!(windows.host, "npipe:////./pipe/docker_engine");
        assert_eq!(windows.reason, Reason::PlatformDefault);
    }

    #[test]
    fn no_config_means_platform_default() {
        let resolved = resolve(&inputs(None, None, None), lookup).unwrap();
        assert_eq!(resolved.reason, Reason::PlatformDefault);
    }

    #[test]
    fn unknown_context_names_the_context() {
        let error = resolve(&inputs(None, None, Some("gone")), lookup).unwrap_err();
        assert!(format!("{error:#}").contains("`gone` does not exist"));
    }

    #[test]
    fn unreadable_context_metadata_is_an_error() {
        let error = resolve(&inputs(None, None, Some("broken")), lookup).unwrap_err();
        assert!(format!("{error:#}").contains("unreadable"));
    }

    #[test]
    fn tls_context_is_refused_by_name() {
        let error = resolve(&inputs(None, Some("remote"), None), lookup).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("`remote`") && text.contains("TLS"));
    }

    #[test]
    fn unsupported_scheme_is_refused() {
        let error = resolve(&inputs(None, Some("weird"), None), lookup).unwrap_err();
        assert!(format!("{error:#}").contains("ssh://me@box"));
    }

    #[test]
    fn context_folder_is_the_sha256_of_its_name() {
        assert_eq!(
            context_dir_name("desktop-linux"),
            "fe9c6bd7a66301f49ca9b6a70b217107cd1284598bfc254700c989b916da791e"
        );
    }

    #[test]
    fn config_and_metadata_files_are_parsed() {
        assert_eq!(
            parse_current_context(r#"{"auths":{},"currentContext":"colima"}"#),
            Some("colima".to_owned())
        );
        assert_eq!(parse_current_context(r#"{"currentContext":""}"#), None);
        assert_eq!(parse_current_context("not json"), None);
        assert_eq!(
            parse_context_endpoint(
                r#"{"Name":"c","Endpoints":{"docker":{"Host":"unix:///a.sock","SkipTLSVerify":false}}}"#,
                false
            )
            .unwrap(),
            context("unix:///a.sock")
        );
        assert!(parse_context_endpoint(r#"{"Endpoints":{}}"#, false).is_err());
    }

    #[test]
    fn context_is_read_from_a_config_folder() {
        let root = std::env::temp_dir().join(format!("docker-host-test-{}", std::process::id()));
        let dir = root
            .join("contexts")
            .join("meta")
            .join(context_dir_name("colima"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"Endpoints":{"docker":{"Host":"unix:///c.sock"}}}"#,
        )
        .unwrap();
        let found = read_context(&root, "colima").unwrap();
        let missing = read_context(&root, "other").unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found, Some(context("unix:///c.sock")));
        assert_eq!(missing, None);
    }
}
