use computer_protocol::{ActReply, Observation};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

const PNG_MIME: &str = "image/png";
const UNCHANGED_NOTE: &str =
    "Screen unchanged. The previous screenshot remains valid; this identical frame was omitted.";
const CHANGED_NOTE: &str = "Screenshot of your screen.";

/// The tool result for an observation: the image when the frame is new, and the metadata as text.
pub fn tool_result(observation: Observation) -> CallToolResult {
    let note = if observation.png_base64.is_some() {
        CHANGED_NOTE
    } else {
        UNCHANGED_NOTE
    };
    let details = json!({
        "frameId": observation.frame_id,
        "capturedAt": observation.captured_at,
        "width": observation.width,
        "height": observation.height,
        "cursor": { "x": observation.cursor.x, "y": observation.cursor.y },
        "activeWindow": observation.active_window,
    });
    let mut content = vec![ContentBlock::text(format!("{note}\n{details}"))];
    if let Some(png) = observation.png_base64 {
        content.push(ContentBlock::image(png, PNG_MIME));
    }
    CallToolResult::success(content)
}

/// The tool result for a batch of actions: a count, then the closing screenshot when there is one.
pub fn act_result(reply: ActReply) -> CallToolResult {
    let ran = format!(
        "Ran {} action{}.",
        reply.actions_run,
        if reply.actions_run == 1 { "" } else { "s" }
    );
    match reply.observation {
        Some(observation) => {
            let mut result = tool_result(observation);
            result.content.insert(0, ContentBlock::text(ran));
            result
        }
        None => CallToolResult::success(vec![ContentBlock::text(format!(
            "{ran} No screenshot was requested."
        ))]),
    }
}

#[cfg(test)]
mod tests {
    use computer_protocol::Cursor;

    use super::*;

    fn observation(png_base64: Option<&str>) -> Observation {
        Observation {
            frame_id: 7,
            captured_at: "2026-10-03T12:00:00Z".to_owned(),
            width: 1280,
            height: 800,
            cursor: Cursor { x: 10, y: 20 },
            active_window: "Terminal".to_owned(),
            png_base64: png_base64.map(str::to_owned),
        }
    }

    fn as_json(result: &CallToolResult) -> serde_json::Value {
        serde_json::to_value(result).unwrap()
    }

    #[test]
    fn new_frame_returns_the_image_and_metadata() {
        let result = as_json(&tool_result(observation(Some("QUJD"))));
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["data"], "QUJD");
        assert_eq!(content[1]["mimeType"], "image/png");
        let text = content[0]["text"].as_str().unwrap();
        let (_, details) = text.split_once('\n').unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(details).unwrap(),
            json!({
                "frameId": 7,
                "capturedAt": "2026-10-03T12:00:00Z",
                "width": 1280,
                "height": 800,
                "cursor": { "x": 10, "y": 20 },
                "activeWindow": "Terminal",
            })
        );
    }

    #[test]
    fn unchanged_frame_omits_the_image_and_says_the_old_one_is_valid() {
        let result = as_json(&tool_result(observation(None)));
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("previous screenshot remains valid")
        );
    }

    #[test]
    fn act_result_counts_actions_before_the_screenshot() {
        let reply = |observation| ActReply {
            actions_run: 2,
            observation,
        };
        let with_image = as_json(&act_result(reply(Some(observation(Some("QUJD"))))));
        let content = with_image["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["text"], "Ran 2 actions.");
        assert_eq!(content[2]["type"], "image");

        let without = as_json(&act_result(reply(None)));
        assert_eq!(without["content"].as_array().unwrap().len(), 1);
    }
}
