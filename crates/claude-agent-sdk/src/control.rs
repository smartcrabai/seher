//! Control protocol over the same stream-json channel.
//!
//! The CLI multiplexes two kinds of frames on stdout:
//!
//! - Regular messages (`type` in `"user" | "assistant" | "system" | "result" |
//!   "stream_event"`) -- surfaced as a [`Message`](crate::types::Message) to
//!   the caller.
//! - **Control requests** (`type == "control_request"`) -- requests for the SDK
//!   to do something *before* the next user turn proceeds: dispatch an MCP
//!   tool call, ask the user whether a tool may run, fire a hook, etc.
//!
//! For each control request the SDK must write back a matching
//! `control_response` on stdin. The CLI blocks until the response arrives, so
//! responses must be prompt.
//!
//! This module defines the [`ControlHandler`] trait the transport uses to
//! route requests, plus the frame types we serialize on the wire.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A `control_request` frame extracted from stdout.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    /// The inner `request` object; its `subtype` field discriminates between
    /// `mcp_message` / `can_use_tool` / `hook_callback` / etc.
    pub request: serde_json::Value,
}

/// What the handler decided to return for a given request.
#[derive(Debug, Clone)]
pub enum ControlResponse {
    Success(serde_json::Value),
    Error(String),
}

impl ControlResponse {
    /// Serialize into the wire frame that gets written to stdin.
    #[must_use]
    pub fn into_frame(self, request_id: &str) -> serde_json::Value {
        match self {
            Self::Success(response) => serde_json::json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": response,
                },
            }),
            Self::Error(error) => serde_json::json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": error,
                },
            }),
        }
    }
}

/// Async handler invoked by the transport for every `control_request` frame.
///
/// The transport already pulled out `request_id`; the handler only needs to
/// look at `request` (which carries `subtype` plus the payload) and return a
/// [`ControlResponse`]. Returning [`ControlResponse::Error`] tells the CLI
/// the request failed -- it then aborts the in-flight tool call (or whatever
/// triggered the request) and yields a normal error to the user.
#[async_trait]
pub trait ControlHandler: Send + Sync + 'static {
    async fn handle(&self, request: ControlRequest) -> ControlResponse;
}

/// Helper that pulls the `subtype` field out of a control request body.
#[must_use]
pub fn request_subtype(request: &serde_json::Value) -> &str {
    request
        .get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Sentinel frame shapes for tests / examples that want to assemble requests
/// by hand without depending on the full `mcp` schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMessageRequest {
    pub subtype: String,
    pub server_name: String,
    pub message: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_frame_contains_required_fields() {
        let f = ControlResponse::Success(serde_json::json!({"ok": 1})).into_frame("req-1");
        assert_eq!(f["type"], "control_response");
        assert_eq!(f["response"]["subtype"], "success");
        assert_eq!(f["response"]["request_id"], "req-1");
        assert_eq!(f["response"]["response"]["ok"], 1);
    }

    #[test]
    fn error_frame_uses_error_subtype() {
        let f = ControlResponse::Error("boom".into()).into_frame("req-2");
        assert_eq!(f["response"]["subtype"], "error");
        assert_eq!(f["response"]["error"], "boom");
    }

    #[test]
    fn request_subtype_extracts_field() {
        let r = serde_json::json!({"subtype": "mcp_message", "server_name": "s"});
        assert_eq!(request_subtype(&r), "mcp_message");
        assert_eq!(request_subtype(&serde_json::json!({})), "");
    }
}
