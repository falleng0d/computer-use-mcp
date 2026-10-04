use bollard::models::ImageInspect;
use chrono::{DateTime, FixedOffset};
use semver::Version;

/// Image label that holds the release version the image was built for.
const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// What the upgrade decision knows about an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageFacts {
    id: String,
    /// Release version from [`VERSION_LABEL`], `None` for dev builds and images without the label.
    pub(crate) version: Option<String>,
    created: Option<DateTime<FixedOffset>>,
}

impl ImageFacts {
    pub(crate) fn from_inspect(inspect: &ImageInspect) -> Option<Self> {
        let version = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(VERSION_LABEL))
            .filter(|version| !version.is_empty())
            .cloned();
        Some(Self {
            id: inspect.id.clone()?,
            version,
            created: inspect
                .created
                .as_deref()
                .and_then(|created| DateTime::parse_from_rfc3339(created).ok()),
        })
    }

    /// The image ID without the `sha256:` prefix, cut to 12 characters.
    pub(crate) fn short_id(&self) -> &str {
        let id = self.id.strip_prefix("sha256:").unwrap_or(&self.id);
        id.get(..12).unwrap_or(id)
    }
}

/// How two images with different IDs are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compare {
    /// By release version. Used by release builds that run their own published image.
    Version,
    /// By build time. Used by dev builds and when `COMPUTER_USE_IMAGE` names the image.
    Created,
}

fn parse_version(version: &str) -> Option<Version> {
    Version::parse(version.strip_prefix('v').unwrap_or(version)).ok()
}

/// Whether `wanted` is strictly newer than `have`.
///
/// An image without a readable version is older than any image with one. The same image, an older image, and an image that cannot be
/// ordered (a version that is not semver) are never newer, so a computer never moves backwards.
pub(crate) fn is_newer(have: &ImageFacts, wanted: &ImageFacts, by: Compare) -> bool {
    if have.id == wanted.id {
        return false;
    }
    match by {
        Compare::Version => {
            let Some(wanted) = wanted.version.as_deref().and_then(parse_version) else {
                return false;
            };
            match have.version.as_deref() {
                None => true,
                Some(have) => parse_version(have).is_some_and(|have| wanted > have),
            }
        }
        Compare::Created => match (have.created, wanted.created) {
            (Some(have), Some(wanted)) => wanted > have,
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(id: &str, version: Option<&str>, created: Option<&str>) -> ImageFacts {
        ImageFacts {
            id: id.to_owned(),
            version: version.map(str::to_owned),
            created: created.map(|created| DateTime::parse_from_rfc3339(created).unwrap()),
        }
    }

    #[test]
    fn release_builds_move_up_but_never_down() {
        let have = facts("a", Some("0.2.0"), None);
        let by = Compare::Version;
        assert!(is_newer(&have, &facts("b", Some("0.2.1"), None), by));
        assert!(is_newer(&have, &facts("b", Some("0.3.0-beta.1"), None), by));
        assert!(!is_newer(&have, &facts("b", Some("0.2.0"), None), by));
        assert!(!is_newer(&have, &facts("b", Some("0.1.9"), None), by));
        assert!(is_newer(
            &facts("a", Some("0.3.0-beta.1"), None),
            &facts("b", Some("0.3.0"), None),
            by
        ));
        assert!(!is_newer(
            &facts("a", Some("0.3.0"), None),
            &facts("b", Some("0.3.0-beta.2"), None),
            by
        ));
    }

    #[test]
    fn a_missing_version_is_older_but_an_unreadable_one_is_left_alone() {
        let wanted = facts("b", Some("0.1.0"), None);
        assert!(is_newer(&facts("a", None, None), &wanted, Compare::Version));
        assert!(!is_newer(
            &facts("a", Some("garbage"), None),
            &wanted,
            Compare::Version
        ));
        assert!(!is_newer(
            &wanted,
            &facts("a", None, None),
            Compare::Version
        ));
    }

    #[test]
    fn the_same_image_is_never_an_upgrade() {
        let same = facts("a", Some("0.1.0"), Some("2026-10-03T10:00:00Z"));
        assert!(!is_newer(&same, &same, Compare::Version));
        assert!(!is_newer(&same, &same, Compare::Created));
    }

    #[test]
    fn dev_builds_follow_build_time_not_image_id() {
        let have = facts("a", None, Some("2026-10-03T10:00:00.5Z"));
        assert!(is_newer(
            &have,
            &facts("b", None, Some("2026-10-03T10:00:00.75Z")),
            Compare::Created
        ));
        assert!(!is_newer(
            &have,
            &facts("b", None, Some("2026-10-03T09:59:59Z")),
            Compare::Created
        ));
        assert!(!is_newer(&have, &facts("b", None, None), Compare::Created));
    }
}
