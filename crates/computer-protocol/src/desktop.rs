use serde::{Deserialize, Serialize};

/// Body of `POST /sessions/{id}/open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenPathRequest {
    /// An http(s) URL, or a file path that starts at the session's working folder.
    pub path: String,
}

/// Body of `POST /sessions/{id}/launch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchAppRequest {
    /// Application name, such as `browser`, `terminal`, a `.desktop` entry, or a program on `PATH`.
    pub application: String,
    /// Page or file the application opens, when it takes one.
    pub uri: Option<String>,
}
