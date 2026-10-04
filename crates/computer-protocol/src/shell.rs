use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Shell timeout used when `COMPUTER_USE_SHELL_TIMEOUT` is not set, in seconds.
pub const DEFAULT_SHELL_TIMEOUT_SECS: u32 = 120;

/// Longest shell timeout used when `COMPUTER_USE_SHELL_TIMEOUT_MAX` is not set, in seconds.
pub const DEFAULT_SHELL_TIMEOUT_MAX_SECS: u32 = 600;

/// Largest accepted shell timeout setting, in seconds.
pub const LONGEST_SHELL_TIMEOUT_SECS: u32 = 86_400;

/// Why a pair of shell timeouts was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShellTimeoutsError {
    #[error("timeouts must be whole seconds from 1 to {LONGEST_SHELL_TIMEOUT_SECS}")]
    OutOfRange,
    #[error("the default timeout ({default} s) must not exceed the maximum ({max} s)")]
    DefaultAboveMax { default: u32, max: u32 },
}

#[derive(Serialize, Deserialize)]
struct RawShellTimeouts {
    default_secs: u32,
    max_secs: u32,
}

/// How long a shell command may run, fixed when the session is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawShellTimeouts", into = "RawShellTimeouts")]
pub struct ShellTimeouts {
    default_secs: u32,
    max_secs: u32,
}

impl ShellTimeouts {
    pub const DEFAULT: Self = Self {
        default_secs: DEFAULT_SHELL_TIMEOUT_SECS,
        max_secs: DEFAULT_SHELL_TIMEOUT_MAX_SECS,
    };

    /// Checks both values, in seconds.
    ///
    /// # Errors
    ///
    /// Fails when a value is outside `1..=LONGEST_SHELL_TIMEOUT_SECS` or the default exceeds the maximum.
    pub fn new(default_secs: u32, max_secs: u32) -> Result<Self, ShellTimeoutsError> {
        let range = 1..=LONGEST_SHELL_TIMEOUT_SECS;
        if !range.contains(&default_secs) || !range.contains(&max_secs) {
            Err(ShellTimeoutsError::OutOfRange)
        } else if default_secs > max_secs {
            Err(ShellTimeoutsError::DefaultAboveMax {
                default: default_secs,
                max: max_secs,
            })
        } else {
            Ok(Self {
                default_secs,
                max_secs,
            })
        }
    }

    /// Timeout of a call: the agent's request, or the default, held between 1 s and the maximum.
    #[must_use]
    pub fn effective(self, requested_secs: Option<u64>) -> Duration {
        let secs = requested_secs
            .unwrap_or(self.default_secs.into())
            .clamp(1, self.max_secs.into());
        Duration::from_secs(secs)
    }

    #[must_use]
    pub fn max(self) -> Duration {
        Duration::from_secs(self.max_secs.into())
    }
}

impl Default for ShellTimeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<RawShellTimeouts> for ShellTimeouts {
    type Error = ShellTimeoutsError;

    fn try_from(raw: RawShellTimeouts) -> Result<Self, Self::Error> {
        Self::new(raw.default_secs, raw.max_secs)
    }
}

impl From<ShellTimeouts> for RawShellTimeouts {
    fn from(timeouts: ShellTimeouts) -> Self {
        Self {
            default_secs: timeouts.default_secs,
            max_secs: timeouts.max_secs,
        }
    }
}

/// Body of `POST /sessions/{id}/shell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRequest {
    pub command: String,
    /// Seconds the command may run. Clamped to the session's maximum. `None` means the session's default.
    pub timeout_secs: Option<u64>,
}

/// How a shell command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellOutcome {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
    },
    /// The command ran out of time and its whole process group was killed.
    /// The session ended or the computer shut down while the command ran, and its whole process group was killed.
    Cancelled,
    TimedOut {
        after_secs: u64,
    },
}

/// Reply to `POST /sessions/{id}/shell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellReply {
    pub outcome: ShellOutcome,
    pub duration_ms: u64,
    /// Standard output, shortened in the middle when it is very long.
    pub stdout: String,
    /// Standard error, shortened in the middle when it is very long.
    pub stderr: String,
}

/// Body of `POST /sessions/{id}/cwd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetCwdRequest {
    pub path: String,
}

/// Reply to `POST /sessions/{id}/cwd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetCwdReply {
    /// Absolute path of the session's new working folder.
    pub cwd: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_timeouts_are_held_between_one_second_and_the_maximum() {
        let timeouts = ShellTimeouts::new(120, 600).unwrap();
        let secs = |requested| timeouts.effective(requested).as_secs();
        assert_eq!(secs(None), 120);
        assert_eq!(secs(Some(30)), 30);
        assert_eq!(secs(Some(0)), 1);
        assert_eq!(secs(Some(100_000)), 600);
    }

    #[test]
    fn timeout_settings_must_be_in_range_and_the_default_within_the_maximum() {
        assert!(ShellTimeouts::new(600, 600).is_ok());
        assert_eq!(
            ShellTimeouts::new(0, 600),
            Err(ShellTimeoutsError::OutOfRange)
        );
        assert_eq!(
            ShellTimeouts::new(1, LONGEST_SHELL_TIMEOUT_SECS + 1),
            Err(ShellTimeoutsError::OutOfRange)
        );
        assert_eq!(
            ShellTimeouts::new(601, 600),
            Err(ShellTimeoutsError::DefaultAboveMax {
                default: 601,
                max: 600
            })
        );
        let bad = serde_json::json!({ "default_secs": 700, "max_secs": 600 });
        assert!(serde_json::from_value::<ShellTimeouts>(bad).is_err());
    }
}
