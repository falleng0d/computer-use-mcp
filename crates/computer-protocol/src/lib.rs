use serde::{Deserialize, Serialize};

pub mod act;
mod shell;

pub use act::{
    ActReply, ActRequest, Action, ActionError, Button, Direction, Kind, Point, RawAction,
};
pub use shell::{
    DEFAULT_SHELL_TIMEOUT_MAX_SECS, DEFAULT_SHELL_TIMEOUT_SECS, LONGEST_SHELL_TIMEOUT_SECS,
    SetCwdReply, SetCwdRequest, ShellOutcome, ShellReply, ShellRequest, ShellTimeouts,
    ShellTimeoutsError,
};

/// Version of the wire format between the host and `computerd`.
pub const PROTOCOL_VERSION: u32 = 5;

pub const RELEASE_VERSION: Option<&str> = match option_env!("COMPUTER_USE_MCP_VERSION") {
    Some(version) if !version.is_empty() => Some(version),
    _ => None,
};

pub const VERSION: &str = match RELEASE_VERSION {
    Some(version) => version,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

/// Port `computerd` listens on inside the container.
pub const API_PORT: u16 = 7070;

/// Environment variable that carries the API token into the container.
pub const TOKEN_ENV: &str = "COMPUTERD_TOKEN";

/// Longest accepted session title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub protocol_version: u32,
    pub version: String,
}

/// Why a session title was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TitleError {
    #[error("title must not be empty")]
    Empty,
    #[error("title is {0} characters long, the limit is {MAX_TITLE_CHARS}")]
    TooLong(usize),
}

/// Short description of the task a session works on, 1 to 80 characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionTitle(String);

impl SessionTitle {
    /// Trims the text and checks its length.
    ///
    /// # Errors
    ///
    /// Fails when the trimmed text is empty or longer than [`MAX_TITLE_CHARS`].
    pub fn parse(text: &str) -> Result<Self, TitleError> {
        let text = text.trim();
        let len = text.chars().count();
        if len == 0 {
            Err(TitleError::Empty)
        } else if len > MAX_TITLE_CHARS {
            Err(TitleError::TooLong(len))
        } else {
            Ok(Self(text.to_owned()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionTitle {
    type Error = TitleError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<SessionTitle> for String {
    fn from(title: SessionTitle) -> Self {
        title.0
    }
}

/// Smallest accepted screen side, in pixels.
pub const MIN_SCREEN_SIDE: u16 = 320;

/// Largest accepted screen side, in pixels.
pub const MAX_SCREEN_SIDE: u16 = 7680;

/// Screen size in pixels. Sides range from [`MIN_SCREEN_SIDE`] to [`MAX_SCREEN_SIDE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScreenSize {
    width: u16,
    height: u16,
}

/// Why a screen size was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "screen size must look like 1280x800, with each side from {MIN_SCREEN_SIDE} to {MAX_SCREEN_SIDE}"
)]
pub struct ScreenSizeError;

impl ScreenSize {
    pub const DEFAULT: Self = Self {
        width: 1280,
        height: 800,
    };

    /// Parses text such as `1280x800`.
    ///
    /// # Errors
    ///
    /// Fails when the text is not `<width>x<height>` or a side is out of range.
    pub fn parse(text: &str) -> Result<Self, ScreenSizeError> {
        let (width, height) = text.trim().split_once(['x', 'X']).ok_or(ScreenSizeError)?;
        let side = |text: &str| {
            text.parse::<u16>()
                .ok()
                .filter(|side| (MIN_SCREEN_SIDE..=MAX_SCREEN_SIDE).contains(side))
                .ok_or(ScreenSizeError)
        };
        Ok(Self {
            width: side(width)?,
            height: side(height)?,
        })
    }

    #[must_use]
    pub fn width(self) -> u16 {
        self.width
    }

    #[must_use]
    pub fn height(self) -> u16 {
        self.height
    }
}

impl Default for ScreenSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl std::fmt::Display for ScreenSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl TryFrom<String> for ScreenSize {
    type Error = ScreenSizeError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ScreenSize> for String {
    fn from(size: ScreenSize) -> Self {
        size.to_string()
    }
}

/// Body of `POST /sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub title: SessionTitle,
    /// Size of the session's screen, applied when the screen opens.
    pub screen_size: ScreenSize,
    /// Timeouts for the session's shell commands.
    pub shell_timeouts: ShellTimeouts,
}

/// Length of a session id, in hex digits.
pub const SESSION_ID_LEN: usize = 32;

/// The id `computerd` issues for a session: 32 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

/// The text is not a session id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a session id")]
pub struct SessionIdError;

impl SessionId {
    /// Checks that the text has the format `computerd` issues.
    ///
    /// # Errors
    ///
    /// Fails when the text is not exactly 32 lowercase hex digits.
    pub fn parse(text: &str) -> Result<Self, SessionIdError> {
        let valid = text.len() == SESSION_ID_LEN
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(SessionIdError)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = SessionIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reply to `POST /sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub session: SessionId,
}

/// Pointer position on the screen, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: i16,
    pub y: i16,
}

/// Reply to `POST /sessions/{id}/observe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Changes whenever the screen content changed since the previous frame.
    pub frame_id: u64,
    /// Capture time as an RFC 3339 UTC timestamp.
    pub captured_at: String,
    pub width: u16,
    pub height: u16,
    pub cursor: Cursor,
    /// Title of the focused window, empty when there is none.
    pub active_window: String,
    /// Base64 PNG of the screen. `None` when the frame is the one the session saw last.
    pub png_base64: Option<String>,
}

/// Body of every error reply from `computerd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_limits_count_characters_after_trimming() {
        let longest = "é".repeat(MAX_TITLE_CHARS);
        assert_eq!(
            SessionTitle::parse(&format!("  {longest}  "))
                .unwrap()
                .as_str(),
            longest
        );
        assert_eq!(
            SessionTitle::parse(&format!("{longest}x")),
            Err(TitleError::TooLong(MAX_TITLE_CHARS + 1))
        );
        assert_eq!(SessionTitle::parse(" \t"), Err(TitleError::Empty));
    }

    #[test]
    fn session_ids_must_be_exactly_32_lowercase_hex_digits() {
        let good = "0123456789abcdef0123456789abcdef";
        assert_eq!(SessionId::parse(good).unwrap().as_str(), good);
        for bad in [
            "",
            "../health",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde/",
        ] {
            assert_eq!(SessionId::parse(bad), Err(SessionIdError), "{bad}");
        }
    }

    #[test]
    fn screen_size_parses_width_by_height_within_limits() {
        let size = ScreenSize::parse(" 1920x1080 ").unwrap();
        assert_eq!((size.width(), size.height()), (1920, 1080));
        assert_eq!(size.to_string(), "1920x1080");
        for bad in [
            "",
            "1280",
            "1280x",
            "x800",
            "1280x800x2",
            "319x800",
            "1280x7681",
            "-1x800",
            "axb",
        ] {
            assert_eq!(ScreenSize::parse(bad), Err(ScreenSizeError), "{bad}");
        }
    }

    #[test]
    fn request_body_with_a_bad_title_is_refused() {
        let body = serde_json::json!({ "title": "", "screen_size": "1280x800", "shell_timeouts": { "default_secs": 120, "max_secs": 600 } });
        assert!(serde_json::from_value::<CreateSession>(body).is_err());
    }
}
