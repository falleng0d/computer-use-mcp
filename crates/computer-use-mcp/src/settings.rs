//! Every `COMPUTER_USE_*` setting, read from the environment and parsed into typed values.
//!
//! An invalid value stays a message in the `Result`, so only the call that needs the setting
//! fails and tells the agent which variable to fix.

use std::num::NonZeroU32;

use computer_protocol::{
    DEFAULT_IDLE_SECS, DEFAULT_PORT_BASE, DEFAULT_SHELL_TIMEOUT_MAX_SECS,
    DEFAULT_SHELL_TIMEOUT_SECS, SCREEN_COUNT, ScreenSize, ShellTimeouts,
};

use crate::open::Mode;

const NAME_ENV: &str = "COMPUTER_USE_NAME";
pub(crate) const PORT_BASE_ENV: &str = "COMPUTER_USE_PORT_BASE";
const IMAGE_ENV: &str = "COMPUTER_USE_IMAGE";
const OPEN_ENV: &str = "COMPUTER_USE_OPEN";
pub(crate) const VNC_VIEWER_ENV: &str = "COMPUTER_USE_VNC_VIEWER";
const SCREEN_SIZE_ENV: &str = "COMPUTER_USE_SCREEN_SIZE";
const SHELL_TIMEOUT_ENV: &str = "COMPUTER_USE_SHELL_TIMEOUT";
const SHELL_TIMEOUT_MAX_ENV: &str = "COMPUTER_USE_SHELL_TIMEOUT_MAX";
const IDLE_TIMEOUT_ENV: &str = "COMPUTER_USE_IDLE_TIMEOUT";
const DEVTOOLS_IDLE_ENV: &str = "COMPUTER_USE_DEVTOOLS_IDLE";
const DEFAULT_DEVTOOLS_IDLE_SECS: u32 = 600;
const TIME_FORMS: &str = "such as 90, 30s, 30m, or 2h";

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A non-empty value with surrounding spaces removed.
fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

pub(crate) fn name() -> Option<String> {
    var(NAME_ENV).filter(|name| !name.is_empty())
}

pub(crate) fn image_override() -> Option<String> {
    var(IMAGE_ENV).filter(|value| !value.is_empty())
}

pub(crate) fn vnc_viewer() -> Option<String> {
    var(VNC_VIEWER_ENV)
}

/// Reads the port base, which must leave room for the viewer page and every screen's VNC port.
pub(crate) fn parse_port_base(value: Option<&str>) -> Result<u16, String> {
    let Some(value) = present(value) else {
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

pub(crate) fn port_base() -> Result<u16, String> {
    parse_port_base(var(PORT_BASE_ENV).as_deref())
}

/// Reads where a new screen shows up. Unset or empty gives the default.
fn parse_mode(value: Option<&str>) -> Result<Mode, String> {
    let Some(text) = present(value) else {
        return Ok(Mode::default());
    };
    match text.to_ascii_lowercase().as_str() {
        "browser" => Ok(Mode::Browser),
        "vnc" => Ok(Mode::Vnc),
        "none" => Ok(Mode::None),
        _ => Err(format!("{OPEN_ENV}={text}: expected browser, vnc, or none")),
    }
}

pub(crate) fn open_mode() -> Result<Mode, String> {
    parse_mode(var(OPEN_ENV).as_deref())
}

pub(crate) fn screen_size() -> Result<ScreenSize, String> {
    match var(SCREEN_SIZE_ENV) {
        Some(text) if !text.is_empty() => {
            ScreenSize::parse(&text).map_err(|error| format!("{SCREEN_SIZE_ENV}={text}: {error}"))
        }
        _ => Ok(ScreenSize::default()),
    }
}

/// Reads a time such as `90`, `90s`, `30m`, or `2h` as seconds. Zero is allowed.
fn parse_secs(text: &str) -> Option<u32> {
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    digits.parse::<u32>().ok()?.checked_mul(factor)
}

/// Seconds from a setting, `None` when unset or empty.
fn secs_setting(name: &str, value: Option<&str>) -> Result<Option<u32>, String> {
    present(value).map_or(Ok(None), |text| {
        parse_secs(text)
            .map(Some)
            .ok_or_else(|| format!("{name}={text}: expected a time {TIME_FORMS}"))
    })
}

/// Reads a time above zero from `name`. Unset or empty gives `default_secs`.
fn nonzero_secs(name: &str, default_secs: u32, value: Option<&str>) -> Result<NonZeroU32, String> {
    match secs_setting(name, value)? {
        None => Ok(NonZeroU32::new(default_secs).expect("the default time is not zero")),
        Some(secs) => NonZeroU32::new(secs)
            .ok_or_else(|| format!("{name}=0: expected a time above zero {TIME_FORMS}")),
    }
}

/// Reads the idle time. Unset or empty gives the default.
pub(crate) fn parse_idle(value: Option<&str>) -> Result<NonZeroU32, String> {
    nonzero_secs(IDLE_TIMEOUT_ENV, DEFAULT_IDLE_SECS, value)
}

pub(crate) fn idle() -> Result<NonZeroU32, String> {
    parse_idle(var(IDLE_TIMEOUT_ENV).as_deref())
}

/// Reads the time after which an unused developer tools session stops. Unset or empty gives 10 minutes.
pub(crate) fn parse_devtools_idle(value: Option<&str>) -> Result<NonZeroU32, String> {
    nonzero_secs(DEVTOOLS_IDLE_ENV, DEFAULT_DEVTOOLS_IDLE_SECS, value)
}

pub(crate) fn devtools_idle() -> Result<NonZeroU32, String> {
    parse_devtools_idle(var(DEVTOOLS_IDLE_ENV).as_deref())
}

/// Builds the shell timeouts from the two settings. A default left unset follows a lower maximum.
fn shell_timeouts_from(default: Option<&str>, max: Option<&str>) -> Result<ShellTimeouts, String> {
    let default = secs_setting(SHELL_TIMEOUT_ENV, default)?;
    let max = secs_setting(SHELL_TIMEOUT_MAX_ENV, max)?;
    let max_secs = max.unwrap_or(DEFAULT_SHELL_TIMEOUT_MAX_SECS);
    let default_secs = default.unwrap_or_else(|| DEFAULT_SHELL_TIMEOUT_SECS.min(max_secs));
    ShellTimeouts::new(default_secs, max_secs)
        .map_err(|error| format!("{SHELL_TIMEOUT_ENV} and {SHELL_TIMEOUT_MAX_ENV}: {error}"))
}

pub(crate) fn shell_timeouts() -> Result<ShellTimeouts, String> {
    shell_timeouts_from(
        var(SHELL_TIMEOUT_ENV).as_deref(),
        var(SHELL_TIMEOUT_MAX_ENV).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_open_setting_defaults_to_browser_and_rejects_unknown_values() {
        assert_eq!(parse_mode(None), Ok(Mode::Browser));
        assert_eq!(parse_mode(Some("")), Ok(Mode::Browser));
        assert_eq!(parse_mode(Some(" VNC ")), Ok(Mode::Vnc));
        assert_eq!(parse_mode(Some("none")), Ok(Mode::None));
        let error = parse_mode(Some("tab")).unwrap_err();
        assert!(error.starts_with("COMPUTER_USE_OPEN=tab"), "{error}");
    }

    #[test]
    fn idle_time_accepts_seconds_minutes_and_hours_and_refuses_the_rest() {
        let secs = |text| parse_idle(text).map(NonZeroU32::get);
        assert_eq!(secs(None), Ok(3600));
        assert_eq!(secs(Some("")), Ok(3600));
        assert_eq!(secs(Some("90")), Ok(90));
        assert_eq!(secs(Some(" 90s ")), Ok(90));
        assert_eq!(secs(Some("30m")), Ok(1800));
        assert_eq!(secs(Some("2h")), Ok(7200));
        for bad in ["0", "0m", "m", "-5", "1d", "1.5h", "4294967295h"] {
            let message = secs(Some(bad)).unwrap_err();
            assert!(message.contains(IDLE_TIMEOUT_ENV), "{bad}: {message}");
        }
    }

    #[test]
    fn devtools_idle_defaults_to_ten_minutes_and_refuses_zero_and_junk() {
        let secs = |text| parse_devtools_idle(text).map(NonZeroU32::get);
        assert_eq!(secs(None), Ok(600));
        assert_eq!(secs(Some("")), Ok(600));
        assert_eq!(secs(Some("45s")), Ok(45));
        assert_eq!(secs(Some("1h")), Ok(3600));
        for bad in ["0", "soon"] {
            let message = secs(Some(bad)).unwrap_err();
            assert!(message.starts_with(DEVTOOLS_IDLE_ENV), "{bad}: {message}");
        }
    }

    #[test]
    fn shell_timeout_settings_default_validate_and_accept_units() {
        let secs = |default, max| {
            shell_timeouts_from(default, max)
                .map(|timeouts| (timeouts.effective(None).as_secs(), timeouts.max().as_secs()))
        };
        assert_eq!(secs(None, None), Ok((120, 600)));
        assert_eq!(secs(Some("30"), Some("90")), Ok((30, 90)));
        assert_eq!(secs(Some("2m"), Some("1h")), Ok((120, 3600)));
        assert_eq!(secs(None, Some("60")), Ok((60, 60)));
        assert_eq!(secs(Some(""), Some("")), Ok((120, 600)));
        let above = secs(Some("700"), None).unwrap_err();
        assert!(above.contains("must not exceed"), "{above}");
        let junk = secs(Some("2d"), None).unwrap_err();
        assert!(junk.starts_with("COMPUTER_USE_SHELL_TIMEOUT=2d"), "{junk}");
        assert!(secs(Some("0"), None).is_err());
    }
}
