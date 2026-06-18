//! Helpers shared by `query()` and `ClaudeSDKClient`.

use serde_json::json;

use crate::errors::Result;
use crate::transport::Transport;

/// Build a stream-json frame for a user message.
#[must_use]
pub fn user_message_frame(text: &str, session_id: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "parent_tool_use_id": null,
        "session_id": session_id,
    })
    .to_string()
}

/// Send a string prompt over an already-connected streaming transport.
pub(crate) async fn send_user_text(
    transport: &mut dyn Transport,
    text: &str,
    session_id: &str,
) -> Result<()> {
    let frame = user_message_frame(text, session_id);
    transport.write(&frame).await
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    #[test]
    fn frame_contains_required_fields() {
        let s = user_message_frame("hi", "sess");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi");
        assert_eq!(v["session_id"], "sess");
        assert!(v["parent_tool_use_id"].is_null());
    }
}
