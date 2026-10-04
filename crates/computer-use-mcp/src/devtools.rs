//! Fixed shell commands and texts for the developer tools.

use std::num::NonZeroU32;

use computer_protocol::{ShellOutcome, ShellReply};

/// Seconds the start and stop commands may run.
pub(crate) const COMMAND_TIMEOUT_SECS: u64 = 60;
/// Seconds the cleanup at the end of a session may take.
pub(crate) const CLEANUP_TIMEOUT_SECS: u64 = 20;
const SCREEN_PREFIX: &str = "screen: ";

/// The command that starts the developer tools for the session's screen. Only the typed idle time varies.
pub(crate) fn start_command(idle_secs: NonZeroU32) -> String {
    format!("CDT_IDLE_SECS={idle_secs} chrome-devtools start")
}

pub(crate) const STOP_COMMAND: &str = "chrome-devtools stop";

/// Screen number from the last `screen: N` line the start command printed.
fn screen_of(stdout: &str) -> Option<u8> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(SCREEN_PREFIX))
        .and_then(|number| number.trim().parse().ok())
}

/// How to use the developer tools on screen `screen`.
pub(crate) fn usage(screen: u8) -> String {
    format!(
        "Chrome DevTools is running for your screen's Chromium as the mcpc session @cdt-{screen}.\n\
         Call a tool with the shell tool: mcpc @cdt-{screen} tools-call <tool> '<json args>'. Pass the arguments as one JSON string.\n\
         Start with `mcpc @cdt-{screen} tools-call list_pages '{{}}'`, then `take_snapshot`. Every tool needs a pageId from list_pages.\n\
         `mcpc @cdt-{screen} tools-list` shows all tools and `mcpc @cdt-{screen} tools-get <name>` shows one schema.\n\
         Useful tools: navigate_page, evaluate_script, take_snapshot, list_console_messages, get_console_message, list_network_requests, get_network_request. When headers or bodies are long, pass requestFilePath or pipe the output to a file.\n\
         DevTools stops by itself after the idle time without calls, and the next mcpc call starts it again with the page state kept. It uses memory only while it runs, so call stop_chrome_devtools when you are done.\n\
         The screen's Chromium has no sandbox and uBlock Origin Lite is on, so ad requests show as blocked."
    )
}

/// What the agent reads when the command did not exit with code 0.
fn failure(reply: &ShellReply, what: &str) -> anyhow::Error {
    let how = match reply.outcome {
        ShellOutcome::Exited { code } => format!("exit code {code}"),
        ShellOutcome::Signaled { signal } => format!("killed by signal {signal}"),
        ShellOutcome::TimedOut { after_secs } => format!("timed out after {after_secs} s"),
        ShellOutcome::Cancelled => "cancelled".to_owned(),
    };
    anyhow::anyhow!(
        "{what} failed ({how})\n{}\n{}",
        reply.stdout.trim(),
        reply.stderr.trim()
    )
}

fn succeeded(reply: &ShellReply) -> bool {
    matches!(reply.outcome, ShellOutcome::Exited { code: 0 })
}

/// The usage text, or the failure of the start command.
pub(crate) fn started(reply: &ShellReply) -> anyhow::Result<String> {
    if !succeeded(reply) {
        return Err(failure(reply, "starting Chrome DevTools"));
    }
    let screen = screen_of(&reply.stdout)
        .ok_or_else(|| anyhow::anyhow!("Chrome DevTools started but did not report its screen"))?;
    Ok(usage(screen))
}

/// The one-line outcome of the stop command, or its failure.
pub(crate) fn stopped(reply: &ShellReply) -> anyhow::Result<String> {
    if !succeeded(reply) {
        return Err(failure(reply, "stopping Chrome DevTools"));
    }
    Ok(reply.stdout.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(outcome: ShellOutcome, stdout: &str) -> ShellReply {
        ShellReply {
            outcome,
            duration_ms: 10,
            stdout: stdout.to_owned(),
            stderr: "boom".to_owned(),
        }
    }

    #[test]
    fn the_start_command_carries_only_the_typed_idle_time() {
        let idle = NonZeroU32::new(600).unwrap();
        assert_eq!(
            start_command(idle),
            "CDT_IDLE_SECS=600 chrome-devtools start"
        );
    }

    #[test]
    fn the_usage_names_the_session_of_the_screen() {
        let text = usage(7);
        assert!(text.contains("mcpc @cdt-7 tools-call list_pages"), "{text}");
        assert!(!text.contains("@cdt-3"), "{text}");
    }

    #[test]
    fn a_started_session_returns_the_usage_for_its_screen() {
        let ok = reply(
            ShellOutcome::Exited { code: 0 },
            "chrome-devtools: @cdt-4 started\nscreen: 4\n",
        );
        assert_eq!(started(&ok).unwrap(), usage(4));
    }

    #[test]
    fn a_failed_start_shows_the_code_and_both_streams() {
        let bad = reply(ShellOutcome::Exited { code: 1 }, "no screen");
        let message = started(&bad).unwrap_err().to_string();
        assert!(message.contains("exit code 1"), "{message}");
        assert!(
            message.contains("no screen") && message.contains("boom"),
            "{message}"
        );
        let timed_out = reply(ShellOutcome::TimedOut { after_secs: 60 }, "");
        assert!(started(&timed_out).is_err());
    }

    #[test]
    fn a_start_without_a_screen_line_is_an_error() {
        let odd = reply(ShellOutcome::Exited { code: 0 }, "started\n");
        assert!(started(&odd).is_err());
    }

    #[test]
    fn stop_passes_the_scripts_outcome_through() {
        let ok = reply(
            ShellOutcome::Exited { code: 0 },
            "chrome-devtools: @cdt-2 was not running\n",
        );
        assert_eq!(
            stopped(&ok).unwrap(),
            "chrome-devtools: @cdt-2 was not running"
        );
        assert!(stopped(&reply(ShellOutcome::Exited { code: 2 }, "")).is_err());
    }
}
