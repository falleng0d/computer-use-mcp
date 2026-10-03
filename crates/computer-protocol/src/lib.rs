use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

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

/// Body of `POST /sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub title: SessionTitle,
}

/// Reply to `POST /sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub session: String,
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
    fn request_body_with_a_bad_title_is_refused() {
        let body = serde_json::json!({ "title": "" });
        assert!(serde_json::from_value::<CreateSession>(body).is_err());
    }
}
