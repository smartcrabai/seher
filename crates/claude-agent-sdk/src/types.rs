//! Message, content-block, and option types -- the public data model.
//!
//! Mirrors `claude_agent_sdk.types` with a Rust-flavored API. Only the fields
//! we actually consume or forward are modeled strictly; everything else is
//! captured under `extra` (`serde_json::Value`) so unknown fields from newer
//! CLI versions round-trip without breaking the parser.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::errors::{ClaudeSDKError, Result};

/// Permission modes accepted by the `claude` CLI (`--permission-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    DontAsk,
    Auto,
}

impl PermissionMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
        }
    }
}

/// System prompt configuration (`--system-prompt` / `--system-prompt-file` /
/// `--append-system-prompt`).
#[derive(Debug, Clone)]
pub enum SystemPrompt {
    Inline(String),
    File(PathBuf),
    Append(String),
    Preset(String),
}

/// MCP server configuration entry.
///
/// We keep this as an opaque JSON value because the CLI accepts many variants
/// (stdio / sse / http / sdk) and the schema evolves. Constructors below cover
/// the common cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpServerConfig(pub serde_json::Value);

impl McpServerConfig {
    /// `{ "type": "stdio", "command": ..., "args": [...], "env": {...} }`
    pub fn stdio(command: impl Into<String>, args: &[String]) -> Self {
        Self(serde_json::json!({
            "type": "stdio",
            "command": command.into(),
            "args": args,
        }))
    }

    /// `{ "type": "sse", "url": ..., "headers": {...} }`
    pub fn sse(url: impl Into<String>) -> Self {
        Self(serde_json::json!({
            "type": "sse",
            "url": url.into(),
        }))
    }

    /// `{ "type": "http", "url": ..., "headers": {...} }`
    pub fn http(url: impl Into<String>) -> Self {
        Self(serde_json::json!({
            "type": "http",
            "url": url.into(),
        }))
    }
}

/// Configuration for a [`query`](crate::query) call or a [`ClaudeSDKClient`].
///
/// Mirrors the subset of `ClaudeAgentOptions` we currently translate into CLI
/// flags. Unknown / future flags can be plumbed through [`Self::extra_args`]
/// without modifying this struct.
#[derive(Debug, Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "1:1 mapping with CLI boolean flags -- bundling them into an enum hurts ergonomics"
)]
pub struct ClaudeAgentOptions {
    pub system_prompt: Option<SystemPrompt>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub mcp_servers: HashMap<String, McpServerConfig>,
    pub strict_mcp_config: bool,
    pub permission_mode: Option<PermissionMode>,
    pub permission_prompt_tool_name: Option<String>,
    pub continue_conversation: bool,
    pub resume: Option<String>,
    pub session_id: Option<String>,
    pub fork_session: bool,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub betas: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub cli_path: Option<PathBuf>,
    pub settings: Option<String>,
    pub add_dirs: Vec<PathBuf>,
    pub env: HashMap<String, String>,
    pub include_partial_messages: bool,
    pub include_hook_events: bool,
    pub setting_sources: Vec<String>,
    /// Escape hatch for CLI flags we don't model. `extra_args["--my-flag"] =
    /// Some("value".into())` becomes `--my-flag value`; `None` is a bare flag.
    pub extra_args: HashMap<String, Option<String>>,
    pub max_buffer_size: Option<usize>,
    /// Optional in-process MCP toolbox. When set, the transport registers a
    /// `{"type": "sdk", "name": "<toolbox.name>"}` entry under
    /// `--mcp-config` and routes `mcp_message` control requests to it.
    pub sdk_mcp_server: Option<crate::tool::AgentToolbox>,
}

impl ClaudeAgentOptions {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// -- Content blocks ---------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingBlock {
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(default)]
    pub is_error: Option<bool>,
}

/// A single content block inside an [`AssistantMessage`] or [`UserMessage`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    ToolUse(ToolUseBlock),
    ToolResult(ToolResultBlock),
    /// Server-side tool (`server_tool_use`, `server_tool_result`, etc.) or any
    /// future block type we don't model yet.
    #[serde(untagged)]
    Other(serde_json::Value),
}

// -- Messages ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UserMessage {
    pub content: UserContent,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
    pub parent_tool_use_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    pub uuid: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub usage: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct SystemMessage {
    pub subtype: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ResultMessage {
    pub subtype: String,
    pub session_id: Option<String>,
    pub duration_ms: Option<u64>,
    pub duration_api_ms: Option<u64>,
    pub is_error: bool,
    pub num_turns: Option<u32>,
    pub total_cost_usd: Option<f64>,
    pub usage: Option<serde_json::Value>,
    pub result: Option<String>,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub uuid: Option<String>,
    pub session_id: Option<String>,
    pub event: serde_json::Value,
    pub parent_tool_use_id: Option<String>,
}

/// Parsed message yielded by [`query`](crate::query) or
/// [`ClaudeSDKClient::receive_messages`](crate::ClaudeSDKClient::receive_messages).
#[derive(Debug, Clone)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    System(SystemMessage),
    Result(ResultMessage),
    StreamEvent(StreamEvent),
    /// Unknown/forward-compatible: any frame we couldn't route, surfaced as
    /// raw JSON.
    Other(serde_json::Value),
}

impl Message {
    /// Parse a raw stream-json frame into a typed [`Message`].
    ///
    /// # Errors
    ///
    /// Returns [`ClaudeSDKError::MessageParse`] when the frame is missing the
    /// `type` field or has a shape we can't reconcile (e.g. an `assistant`
    /// frame without a `message` object).
    pub fn from_frame(value: serde_json::Value) -> Result<Self> {
        let ty = value
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ClaudeSDKError::MessageParse("missing 'type' field".into()))?;

        match ty {
            "user" => Ok(Self::User(parse_user(&value)?)),
            "assistant" => Ok(Self::Assistant(parse_assistant(&value)?)),
            "system" => Ok(Self::System(parse_system(&value))),
            "result" => Ok(Self::Result(parse_result(value))),
            "stream_event" => Ok(Self::StreamEvent(parse_stream_event(&value))),
            _ => Ok(Self::Other(value)),
        }
    }
}

fn parse_user(value: &serde_json::Value) -> Result<UserMessage> {
    let message = value
        .get("message")
        .ok_or_else(|| ClaudeSDKError::MessageParse("user frame missing 'message'".into()))?;
    let content = match message.get("content") {
        Some(serde_json::Value::String(s)) => UserContent::Text(s.clone()),
        Some(serde_json::Value::Array(arr)) => {
            let blocks: Vec<ContentBlock> = arr
                .iter()
                .map(|b| {
                    serde_json::from_value(b.clone())
                        .unwrap_or_else(|_| ContentBlock::Other(b.clone()))
                })
                .collect();
            UserContent::Blocks(blocks)
        }
        Some(other) => UserContent::Blocks(vec![ContentBlock::Other(other.clone())]),
        None => UserContent::Text(String::new()),
    };
    Ok(UserMessage {
        content,
        session_id: value
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        uuid: value.get("uuid").and_then(|v| v.as_str()).map(String::from),
        parent_tool_use_id: value
            .get("parent_tool_use_id")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_assistant(value: &serde_json::Value) -> Result<AssistantMessage> {
    let message = value
        .get("message")
        .ok_or_else(|| ClaudeSDKError::MessageParse("assistant frame missing 'message'".into()))?;
    let blocks: Vec<ContentBlock> = match message.get("content") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(|b| {
                serde_json::from_value(b.clone()).unwrap_or_else(|_| ContentBlock::Other(b.clone()))
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(AssistantMessage {
        content: blocks,
        model: message
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from),
        session_id: value
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        message_id: message.get("id").and_then(|v| v.as_str()).map(String::from),
        stop_reason: message
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(String::from),
        uuid: value.get("uuid").and_then(|v| v.as_str()).map(String::from),
        parent_tool_use_id: value
            .get("parent_tool_use_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        usage: message.get("usage").cloned(),
    })
}

fn parse_system(value: &serde_json::Value) -> SystemMessage {
    SystemMessage {
        subtype: value
            .get("subtype")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default(),
        data: value.clone(),
    }
}

fn parse_result(value: serde_json::Value) -> ResultMessage {
    let subtype = value
        .get("subtype")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let session_id = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let duration_ms = value.get("duration_ms").and_then(serde_json::Value::as_u64);
    let duration_api_ms = value
        .get("duration_api_ms")
        .and_then(serde_json::Value::as_u64);
    let is_error = value
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let num_turns = value
        .get("num_turns")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let total_cost_usd = value
        .get("total_cost_usd")
        .and_then(serde_json::Value::as_f64);
    let usage = value.get("usage").cloned();
    let result = value
        .get("result")
        .and_then(|v| v.as_str())
        .map(String::from);
    ResultMessage {
        subtype,
        session_id,
        duration_ms,
        duration_api_ms,
        is_error,
        num_turns,
        total_cost_usd,
        usage,
        result,
        raw: value,
    }
}

fn parse_stream_event(value: &serde_json::Value) -> StreamEvent {
    StreamEvent {
        uuid: value.get("uuid").and_then(|v| v.as_str()).map(String::from),
        session_id: value
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        event: value
            .get("event")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        parent_tool_use_id: value
            .get("parent_tool_use_id")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_assistant_with_text_block() {
        let frame = serde_json::json!({
            "type": "assistant",
            "session_id": "s1",
            "message": {
                "id": "msg_1",
                "model": "claude-sonnet-4-6",
                "content": [{"type": "text", "text": "hi"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 5}
            }
        });
        let msg = Message::from_frame(frame).expect("parse");
        match msg {
            Message::Assistant(a) => {
                assert_eq!(a.model.as_deref(), Some("claude-sonnet-4-6"));
                assert_eq!(a.session_id.as_deref(), Some("s1"));
                assert_eq!(a.content.len(), 1);
                match &a.content[0] {
                    ContentBlock::Text(t) => assert_eq!(t.text, "hi"),
                    other => panic!("unexpected block: {other:?}"),
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn parse_assistant_with_tool_use_block() {
        let frame = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"path": "/tmp/x"}
                }]
            }
        });
        let msg = Message::from_frame(frame).expect("parse");
        let Message::Assistant(a) = msg else {
            panic!("expected assistant")
        };
        let ContentBlock::ToolUse(tu) = &a.content[0] else {
            panic!("expected tool_use")
        };
        assert_eq!(tu.id, "tu_1");
        assert_eq!(tu.name, "Read");
    }

    #[test]
    fn parse_user_with_string_content() {
        let frame = serde_json::json!({
            "type": "user",
            "session_id": "s1",
            "message": {"role": "user", "content": "hello"}
        });
        let msg = Message::from_frame(frame).expect("parse");
        let Message::User(u) = msg else {
            panic!("expected user")
        };
        match u.content {
            UserContent::Text(t) => assert_eq!(t, "hello"),
            UserContent::Blocks(_) => panic!("expected text"),
        }
    }

    #[test]
    fn parse_result_basic() {
        let frame = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "session_id": "s1",
            "is_error": false,
            "num_turns": 3,
            "total_cost_usd": 0.01,
            "result": "done"
        });
        let msg = Message::from_frame(frame).expect("parse");
        let Message::Result(r) = msg else {
            panic!("expected result")
        };
        assert_eq!(r.subtype, "success");
        assert_eq!(r.num_turns, Some(3));
        assert_eq!(r.result.as_deref(), Some("done"));
    }

    #[test]
    fn parse_unknown_type_is_other() {
        let frame = serde_json::json!({"type": "future_kind", "foo": 1});
        let msg = Message::from_frame(frame).expect("parse");
        assert!(matches!(msg, Message::Other(_)));
    }

    #[test]
    fn parse_missing_type_errors() {
        let frame = serde_json::json!({"foo": 1});
        let err = Message::from_frame(frame).unwrap_err();
        assert!(matches!(err, ClaudeSDKError::MessageParse(_)));
    }
}
