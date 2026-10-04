//! The environment of processes `computerd` starts for a screen.

const DEFAULT_USER: &str = "computer";
pub(crate) const SHELL: &str = "/bin/bash";

/// Environment of a command: a few variables taken from the daemon's own, never its secrets.
fn environment(
    parent: impl Fn(&str) -> Option<String>,
    display: Option<u8>,
) -> Vec<(&'static str, String)> {
    let user = parent("USER").unwrap_or_else(|| DEFAULT_USER.to_owned());
    let mut env = vec![
        (
            "HOME",
            parent("HOME").unwrap_or_else(|| format!("/home/{DEFAULT_USER}")),
        ),
        ("LOGNAME", user.clone()),
        ("USER", user),
        ("SHELL", SHELL.to_owned()),
        ("TERM", "dumb".to_owned()),
    ];
    for name in ["PATH", "LANG", "LC_ALL", "TZ"] {
        if let Some(value) = parent(name) {
            env.push((name, value));
        }
    }
    if let Some(number) = display {
        env.push(("DISPLAY", format!(":{number}")));
    }
    env
}

/// [`environment`] taken from the daemon's own variables.
pub(crate) fn from_process(display: Option<u8>) -> Vec<(&'static str, String)> {
    environment(|name| std::env::var(name).ok(), display)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_passes_the_login_basics_and_never_the_token() {
        let parent = |name: &str| match name {
            "PATH" => Some("/usr/bin".to_owned()),
            "HOME" => Some("/home/computer".to_owned()),
            "COMPUTERD_TOKEN" => Some("secret".to_owned()),
            _ => None,
        };
        let with_screen = environment(parent, Some(3));
        let get = |env: &[(&str, String)], key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(get(&with_screen, "DISPLAY"), Some(":3".to_owned()));
        assert_eq!(get(&with_screen, "USER"), Some("computer".to_owned()));
        assert_eq!(get(&with_screen, "PATH"), Some("/usr/bin".to_owned()));
        assert_eq!(get(&with_screen, "COMPUTERD_TOKEN"), None);
        assert_eq!(get(&environment(parent, None), "DISPLAY"), None);
    }
}
