use super::types::{ClaudeTerminalResponse, TranscriptMessage};

/// Extract a single text string from a Claude transcript response.
///
/// Priority:
///   1. `last_result_message.result` (final answer from Claude Code)
///   2. Concatenated text blocks from all assistant messages
///   3. Empty string
#[must_use]
pub fn normalize_text(response: &ClaudeTerminalResponse) -> String {
    if let Some(last) = &response.last_result_message
        && last.msg_type == "result"
        && let Some(r) = &last.result
        && !r.is_empty()
    {
        return r.clone();
    }
    response
        .assistant_messages
        .iter()
        .map(text_from_assistant_message)
        .filter(|s| !s.is_empty())
        .collect::<String>()
}

fn text_from_assistant_message(msg: &TranscriptMessage) -> String {
    if msg.msg_type != "assistant" {
        return String::new();
    }
    let content = match &msg.message {
        Some(m) => &m.content,
        None => return String::new(),
    };
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => extract_text_blocks(blocks),
        _ => String::new(),
    }
}

fn extract_text_blocks(blocks: &[serde_json::Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            let obj = block.as_object()?;
            if obj.get("type")?.as_str() != Some("text") {
                return None;
            }
            obj.get("text")?.as_str().map(std::string::ToString::to_string)
        })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_terminal::types::{ClaudeTerminalResponse, MessageContent, TranscriptMessage};
    use serde_json::json;

    fn make_assistant(content: serde_json::Value) -> TranscriptMessage {
        TranscriptMessage {
            msg_type: "assistant".to_string(),
            uuid: None,
            session_id: None,
            subtype: None,
            result: None,
            is_error: None,
            message: Some(MessageContent {
                content: Some(content),
                role: None,
            }),
            extra: std::collections::HashMap::new(),
        }
    }

    fn make_result(text: &str) -> TranscriptMessage {
        TranscriptMessage {
            msg_type: "result".to_string(),
            uuid: None,
            session_id: None,
            subtype: None,
            result: Some(text.to_string()),
            is_error: None,
            message: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn prefers_result_message() {
        let response = ClaudeTerminalResponse {
            session_id: "s1".to_string(),
            assistant_messages: vec![make_assistant(json!("interim text"))],
            last_result_message: Some(make_result("final answer")),
        };
        assert_eq!(normalize_text(&response), "final answer");
    }

    #[test]
    fn falls_back_to_assistant_messages() {
        let response = ClaudeTerminalResponse {
            session_id: "s1".to_string(),
            assistant_messages: vec![make_assistant(json!("hello world"))],
            last_result_message: None,
        };
        assert_eq!(normalize_text(&response), "hello world");
    }

    #[test]
    fn concatenates_text_blocks() {
        let content = json!([
            {"type": "text", "text": "Hello"},
            {"type": "tool_use", "name": "Read"},
            {"type": "text", "text": " world"}
        ]);
        let response = ClaudeTerminalResponse {
            session_id: "s1".to_string(),
            assistant_messages: vec![make_assistant(content)],
            last_result_message: None,
        };
        assert_eq!(normalize_text(&response), "Hello world");
    }

    #[test]
    fn empty_result_falls_through() {
        let mut result_msg = make_result("");
        result_msg.result = Some(String::new());
        let response = ClaudeTerminalResponse {
            session_id: "s1".to_string(),
            assistant_messages: vec![make_assistant(json!("assistant text"))],
            last_result_message: Some(result_msg),
        };
        assert_eq!(normalize_text(&response), "assistant text");
    }

    #[test]
    fn no_messages_returns_empty() {
        let response = ClaudeTerminalResponse {
            session_id: "s1".to_string(),
            assistant_messages: vec![],
            last_result_message: None,
        };
        assert_eq!(normalize_text(&response), "");
    }
}
