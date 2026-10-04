use computer_protocol::{ShellOutcome, ShellReply};
use rmcp::model::{CallToolResult, ContentBlock};

/// Text the agent reads for a finished command.
fn describe(reply: &ShellReply) -> String {
    let how = match reply.outcome {
        ShellOutcome::Exited { code } => format!("exit code: {code}"),
        ShellOutcome::Signaled { signal } => format!("killed by signal {signal}"),
        ShellOutcome::Cancelled => "stopped because the session ended or the computer is shutting down, the command and everything it started were killed".to_owned(),
        ShellOutcome::TimedOut { after_secs } => format!(
            "timed out after {after_secs} s and was killed together with everything it started, the output below is what it printed before that"
        ),
    };
    let seconds = f64::from(u32::try_from(reply.duration_ms).unwrap_or(u32::MAX)) / 1000.0;
    format!(
        "{how}\nduration: {seconds:.1} s\n--- stdout ---\n{}\n--- stderr ---\n{}",
        stream(&reply.stdout),
        stream(&reply.stderr)
    )
}

fn stream(text: &str) -> &str {
    if text.is_empty() { "(empty)" } else { text }
}

/// A command that ran is a normal result, even when it failed. A timeout is flagged as an error.
pub(crate) fn tool_result(reply: &ShellReply) -> CallToolResult {
    let content = vec![ContentBlock::text(describe(reply))];
    if matches!(
        reply.outcome,
        ShellOutcome::TimedOut { .. } | ShellOutcome::Cancelled
    ) {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(outcome: ShellOutcome, stdout: &str) -> ShellReply {
        ShellReply {
            outcome,
            duration_ms: 1500,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    #[test]
    fn a_failed_command_is_a_normal_result_with_both_streams() {
        let mut failed = reply(ShellOutcome::Exited { code: 3 }, "hi\n");
        failed.stderr = "err\n".to_owned();
        assert_eq!(
            describe(&failed),
            "exit code: 3\nduration: 1.5 s\n--- stdout ---\nhi\n\n--- stderr ---\nerr\n"
        );
        assert_ne!(tool_result(&failed).is_error, Some(true));
    }

    #[test]
    fn a_timeout_says_so_and_is_flagged_as_an_error() {
        let timed_out = reply(ShellOutcome::TimedOut { after_secs: 2 }, "");
        assert!(describe(&timed_out).starts_with("timed out after 2 s"));
        assert_eq!(tool_result(&timed_out).is_error, Some(true));
    }
}
