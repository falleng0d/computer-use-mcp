use std::{fmt, time::Duration};

use computer_protocol::OWNER_TIMEOUT_SECS;
use tokio::time::Instant;

/// How long an owner may stay silent before its sessions end.
pub const OWNER_TIMEOUT: Duration = Duration::from_secs(OWNER_TIMEOUT_SECS);

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The agent called `end_session`.
    Agent,
    /// The owner told `computerd` it is shutting down.
    OwnerLeft,
    /// The owner stopped sending heartbeats.
    OwnerGone,
    /// No agent call for the session's idle time.
    Idle(Duration),
}

impl EndReason {
    /// Short label for logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "ended by agent",
            Self::OwnerLeft => "owner left",
            Self::OwnerGone => "owner gone",
            Self::Idle(_) => "idle",
        }
    }
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => f.write_str("it was ended with end_session"),
            Self::OwnerLeft => f.write_str("the MCP server that started it shut down"),
            Self::OwnerGone => write!(
                f,
                "the MCP server that started it stopped responding for {}",
                human(OWNER_TIMEOUT)
            ),
            Self::Idle(idle) => write!(f, "it had no calls for {}", human(*idle)),
        }
    }
}

fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{} h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// What the end decision of one session depends on.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    pub now: Instant,
    pub created: Instant,
    pub last_activity: Instant,
    /// Time of the owner's last heartbeat, `None` when it never sent one.
    pub owner_seen: Option<Instant>,
    pub idle: Duration,
    /// An agent call is running on the session.
    pub call_running: bool,
    /// Viewers attached to the session's screen.
    pub viewers: usize,
}

/// Decides whether a session ends now.
///
/// A silent owner ends the session even while a call runs. A running call counts as
/// activity, so a long command never makes its own session idle. An attached viewer does
/// too, so a session someone watches never ends for being idle.
pub fn end_reason(l: &Liveness) -> Option<EndReason> {
    let owner_last = l.owner_seen.map_or(l.created, |seen| seen.max(l.created));
    if l.now.saturating_duration_since(owner_last) >= OWNER_TIMEOUT {
        return Some(EndReason::OwnerGone);
    }
    if !l.call_running
        && l.viewers == 0
        && l.now.saturating_duration_since(l.last_activity) >= l.idle
    {
        return Some(EndReason::Idle(l.idle));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(3600);

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn liveness(base: Instant, now: u64) -> Liveness {
        Liveness {
            now: at(base, now),
            created: base,
            last_activity: base,
            owner_seen: None,
            idle: IDLE,
            call_running: false,
            viewers: 0,
        }
    }

    #[test]
    fn a_silent_owner_ends_its_session_30_seconds_after_creation() {
        let base = Instant::now();
        assert_eq!(end_reason(&liveness(base, 29)), None);
        assert_eq!(end_reason(&liveness(base, 30)), Some(EndReason::OwnerGone));
    }

    #[test]
    fn heartbeats_push_the_owner_deadline_back() {
        let base = Instant::now();
        let beat = |seen, now| Liveness {
            owner_seen: Some(at(base, seen)),
            ..liveness(base, now)
        };
        assert_eq!(end_reason(&beat(20, 49)), None);
        assert_eq!(end_reason(&beat(20, 50)), Some(EndReason::OwnerGone));
    }

    #[test]
    fn a_session_created_after_the_last_heartbeat_gets_a_fresh_30_seconds() {
        let base = Instant::now();
        let l = Liveness {
            created: at(base, 100),
            owner_seen: Some(at(base, 10)),
            last_activity: at(base, 100),
            ..liveness(base, 129)
        };
        assert_eq!(end_reason(&l), None);
    }

    #[test]
    fn a_session_ends_when_idle_time_passes_without_calls() {
        let base = Instant::now();
        let beat = |activity, now| Liveness {
            owner_seen: Some(at(base, now - 1)),
            last_activity: at(base, activity),
            ..liveness(base, now)
        };
        assert_eq!(end_reason(&beat(100, 3699)), None);
        assert_eq!(end_reason(&beat(100, 3700)), Some(EndReason::Idle(IDLE)));
    }

    #[test]
    fn a_running_call_keeps_the_session_from_going_idle() {
        let base = Instant::now();
        let l = Liveness {
            owner_seen: Some(at(base, 7199)),
            call_running: true,
            ..liveness(base, 7200)
        };
        assert_eq!(end_reason(&l), None);
    }

    #[test]
    fn a_watched_session_never_goes_idle_but_a_silent_owner_still_ends_it() {
        let base = Instant::now();
        let watched = |now| Liveness {
            owner_seen: Some(at(base, now - 1)),
            viewers: 1,
            ..liveness(base, now)
        };
        assert_eq!(end_reason(&watched(7200)), None);
        let unwatched = Liveness {
            viewers: 0,
            ..watched(7200)
        };
        assert_eq!(end_reason(&unwatched), Some(EndReason::Idle(IDLE)));
        let silent = Liveness {
            owner_seen: None,
            ..watched(7200)
        };
        assert_eq!(end_reason(&silent), Some(EndReason::OwnerGone));
    }

    #[test]
    fn a_silent_owner_ends_a_session_even_while_a_call_runs() {
        let base = Instant::now();
        let l = Liveness {
            call_running: true,
            ..liveness(base, 31)
        };
        assert_eq!(end_reason(&l), Some(EndReason::OwnerGone));
    }
}
