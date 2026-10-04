use crate::settings;

pub const REGISTRY_IMAGE: &str = "ghcr.io/falleng0d/computer-use-mcp";
pub const DEV_IMAGE: &str = "computer-use-mcp:dev";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub reference: String,
    pub pull: bool,
    /// Images are ordered by release version, not by build time. True only for a release build
    /// that runs its own published image.
    pub by_version: bool,
}

pub fn select(release_version: Option<&str>, image_override: Option<&str>) -> Image {
    let reference = match (image_override, release_version) {
        (Some(reference), _) => reference.to_owned(),
        (None, Some(version)) => format!("{REGISTRY_IMAGE}:{version}"),
        (None, None) => DEV_IMAGE.to_owned(),
    };
    Image {
        reference,
        pull: release_version.is_some(),
        by_version: release_version.is_some() && image_override.is_none(),
    }
}

pub fn from_env() -> Image {
    select(
        computer_protocol::RELEASE_VERSION,
        settings::image_override().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_build_pulls_its_own_version() {
        assert_eq!(
            select(Some("1.2.0-beta.1"), None),
            Image {
                reference: "ghcr.io/falleng0d/computer-use-mcp:1.2.0-beta.1".to_owned(),
                pull: true,
                by_version: true,
            }
        );
    }

    #[test]
    fn dev_build_uses_local_image_without_pulling() {
        assert_eq!(
            select(None, None),
            Image {
                reference: "computer-use-mcp:dev".to_owned(),
                pull: false,
                by_version: false,
            }
        );
    }

    #[test]
    fn override_keeps_the_build_pull_policy() {
        assert!(!select(None, Some("my/image:x")).pull);
        assert_eq!(
            select(Some("1.2.0"), Some("my/image:x")),
            Image {
                reference: "my/image:x".to_owned(),
                pull: true,
                by_version: false,
            }
        );
    }
}
