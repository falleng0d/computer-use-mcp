use std::num::NonZeroU32;

use des::{
    Des,
    cipher::{BlockCipherEncrypt, KeyInit},
};
use serde::{Deserialize, Serialize};

pub mod act;
mod desktop;
mod files;
mod shell;
mod transfer;

pub use act::{
    ActReply, ActRequest, Action, ActionError, Button, Direction, Kind, Point, RawAction,
};
pub use desktop::{LaunchAppRequest, OpenPathRequest};
pub use files::{
    FileEntry, FileKind, ImageType, ListFilesReply, ListFilesRequest, MAX_IMAGE_BYTES,
    MAX_WRITE_BYTES, ReadFileReply, ReadFileRequest, WriteFileReply, WriteFileRequest,
};
pub use shell::{
    DEFAULT_SHELL_TIMEOUT_MAX_SECS, DEFAULT_SHELL_TIMEOUT_SECS, LONGEST_SHELL_TIMEOUT_SECS,
    SetCwdReply, SetCwdRequest, ShellOutcome, ShellReply, ShellRequest, ShellTimeouts,
    ShellTimeoutsError,
};
pub use transfer::{
    DownloadRequest, MAX_LISTED_SKIPS, SKIP_REPORT_ENTRY, SkipList, Skipped, TransferReply,
    UploadQuery,
};

/// Version of the wire format between the host and `computerd`.
pub const PROTOCOL_VERSION: u32 = 11;

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

/// Environment variable that tells `computerd` which host port its viewer port is published on.
pub const HOST_PORT_BASE_ENV: &str = "COMPUTERD_HOST_PORT_BASE";

/// Host port the viewer page is published on unless the user picks another base.
pub const DEFAULT_PORT_BASE: u16 = 20900;

/// Port of the viewer page and its WebSocket bridge inside the container.
pub const VIEWER_PORT: u16 = 20900;

/// Number of screens the computer has.
pub const SCREEN_COUNT: u8 = 16;

/// Port inside the container where raw VNC for `screen` (1 to [`SCREEN_COUNT`]) is served.
#[must_use]
pub fn vnc_port(screen: u8) -> u16 {
    VIEWER_PORT + u16::from(screen)
}

/// Link that opens the viewer page when the host publishes it on `host_port`.
///
/// The password after `#key=` stays in the browser and is never sent to the server in a request line.
#[must_use]
pub fn viewer_link(host_port: u16, key: &str) -> String {
    format!("http://127.0.0.1:{host_port}/#key={key}")
}

/// Link that opens the viewer page focused on `screen`.
#[must_use]
pub fn viewer_screen_link(host_port: u16, key: &str, screen: u8) -> String {
    format!("http://127.0.0.1:{host_port}/#key={key}&screen={screen}")
}

/// Fixed key VNC uses to obfuscate the password in a password file.
const VNC_FILE_KEY: [u8; 8] = [23, 82, 107, 6, 35, 78, 88, 7];

/// Contents of a VNC password file for `key`, as `Xvnc` and `vncviewer -passwd` read it.
///
/// The key is cut or padded to 8 bytes and DES-encrypted with a fixed key. VNC bit-reverses every byte of a DES key.
#[must_use]
pub fn vnc_password_file(key: &str) -> [u8; 8] {
    let mut block = [0u8; 8];
    for (slot, byte) in block.iter_mut().zip(key.bytes()) {
        *slot = byte;
    }
    let cipher = Des::new(&VNC_FILE_KEY.map(u8::reverse_bits).into());
    let mut out = block.into();
    cipher.encrypt_block(&mut out);
    out.into()
}

/// Longest accepted session title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub protocol_version: u32,
    pub version: String,
}

/// What the host needs to know about the viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerInfo {
    /// Password for the viewer page and for raw VNC.
    pub key: String,
    /// Viewer pages open in a browser now.
    pub pages: usize,
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
    /// The MCP server process that keeps the session alive with heartbeats.
    pub owner: OwnerId,
    /// Seconds without agent calls after which the session ends.
    pub idle_secs: NonZeroU32,
}

/// Seconds between the heartbeats an MCP server sends for its sessions.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// Seconds without a heartbeat after which the sessions of an owner end.
pub const OWNER_TIMEOUT_SECS: u64 = 30;

/// Seconds without agent calls after which a session ends, unless the host sets another time.
pub const DEFAULT_IDLE_SECS: u32 = 3600;

/// Length of an owner id, in hex digits.
pub const OWNER_ID_LEN: usize = 32;

/// Identifies one MCP server process: 32 lowercase hex digits, random per process.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OwnerId(String);

/// The text is not an owner id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an owner id")]
pub struct OwnerIdError;

impl OwnerId {
    /// Checks that the text is exactly 32 lowercase hex digits.
    ///
    /// # Errors
    ///
    /// Fails when the text has another length or other characters.
    pub fn parse(text: &str) -> Result<Self, OwnerIdError> {
        let valid = text.len() == OWNER_ID_LEN
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(OwnerIdError)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OwnerId {
    type Error = OwnerIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<OwnerId> for String {
    fn from(id: OwnerId) -> Self {
        id.0
    }
}

impl std::fmt::Display for OwnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
    /// Screen number this call opened for the session, set on a reply to `observe` and never inside an [`ActReply`].
    #[serde(default)]
    pub opened_screen: Option<u8>,
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
    fn the_password_file_is_the_des_obfuscation_xvnc_reads() {
        assert_eq!(
            vnc_password_file("abcd2345"),
            [255, 232, 190, 74, 23, 18, 52, 125],
            "bytes from the same encoding that Xvnc accepted in the Docker test"
        );
    }

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
