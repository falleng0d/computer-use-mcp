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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub protocol_version: u32,
    pub version: String,
}
