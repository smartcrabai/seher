//! Custom tool definitions (function calling) for the pi runner.
//!
//! A [`SeherTool`] pairs a JSON Schema with a synchronous handler. Before a prompt
//! runs, [`crate::sdk::PiRunner`] converts each one into a `pi::tools::Tool`
//! ([`PiToolAdapter`]) and injects it into the live agent session -- pi's
//! `SessionOptions` has no custom-tool field, so injection happens post-creation
//! via `AgentSessionHandle::session_mut()`.
//!
//! Custom tools only run on the in-process `pi` engine. The `claude-terminal`
//! backend drives the `claude` CLI via tmux and cannot honor them; resolution
//! drops those candidates when tools are requested (see
//! [`crate::sdk::resolve::ResolveOptions::require_tools`]).

use std::sync::Arc;

/// Synchronous tool handler. Receives the raw JSON input the model produced
/// (validation/parsing is the handler's responsibility). `Ok(text)` becomes the
/// tool result; `Err(message)` is surfaced to the model with `is_error: true`
/// so it can recover or retry.
pub type ToolHandler = Arc<dyn Fn(serde_json::Value) -> Result<String, String> + Send + Sync>;

/// A custom tool the model can call: name/description, a JSON Schema
/// (`type: object` with `properties`) describing its input, and the handler
/// invoked with that input.
///
/// Cloning is cheap and shares the handler: all clones invoke the same `Arc`'d
/// closure, so any interior state is shared between them.
#[derive(Clone)]
pub struct SeherTool {
    pub name: String,
    pub description: String,
    /// JSON Schema (`type: object` with `properties`) describing the tool input.
    pub parameters: serde_json::Value,
    pub handler: ToolHandler,
}

impl SeherTool {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        handler: ToolHandler,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler,
        }
    }
}

impl std::fmt::Debug for SeherTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeherTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

/// Adapts a [`SeherTool`] to pi's `Tool` trait so it can be registered on the
/// agent via `Agent::extend_tools`.
pub(crate) struct PiToolAdapter {
    tool: SeherTool,
}

impl PiToolAdapter {
    pub(crate) const fn new(tool: SeherTool) -> Self {
        Self { tool }
    }
}

#[async_trait::async_trait]
impl pi::tools::Tool for PiToolAdapter {
    fn name(&self) -> &str {
        &self.tool.name
    }

    fn label(&self) -> &str {
        &self.tool.name
    }

    fn description(&self) -> &str {
        &self.tool.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.tool.parameters.clone()
    }

    /// Runs the synchronous handler inline. This blocks the executor, which is
    /// fine here: the pi runner drives the whole session with `block_on` on a
    /// dedicated thread (see [`crate::sdk::pi_runner`]).
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(pi::tools::ToolUpdate) + Send + Sync>>,
    ) -> pi::error::Result<pi::tools::ToolOutput> {
        use pi::model::{ContentBlock, TextContent};

        // Handler errors are not pi errors: returning `is_error: true` feeds the
        // failure back to the model (standard function-calling behavior) instead
        // of aborting the turn.
        let (text, is_error) = match (self.tool.handler)(input) {
            Ok(out) => (out, false),
            Err(msg) => (msg, true),
        };
        Ok(pi::tools::ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error,
        })
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;
    use pi::model::ContentBlock;
    use pi::tools::Tool as _;

    fn echo_tool(handler: ToolHandler) -> SeherTool {
        SeherTool::new(
            "echo",
            "Echo the input back",
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
            handler,
        )
    }

    fn text_of(output: &pi::tools::ToolOutput) -> &str {
        match output.content.first().expect("one content block") {
            ContentBlock::Text(t) => &t.text,
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn execute_success_returns_text_not_error() {
        let adapter = PiToolAdapter::new(echo_tool(Arc::new(|input| Ok(format!("got: {input}")))));
        let out = futures::executor::block_on(adapter.execute(
            "call-1",
            serde_json::json!({"text": "hi"}),
            None,
        ))
        .expect("execute succeeds");
        assert!(!out.is_error);
        assert_eq!(text_of(&out), r#"got: {"text":"hi"}"#);
    }

    #[test]
    fn execute_error_sets_is_error() {
        let adapter = PiToolAdapter::new(echo_tool(Arc::new(|_| Err("boom".to_string()))));
        let out =
            futures::executor::block_on(adapter.execute("call-1", serde_json::json!({}), None))
                .expect("execute still returns Ok");
        assert!(out.is_error);
        assert_eq!(text_of(&out), "boom");
    }

    #[test]
    fn adapter_exposes_name_label_and_parameters() {
        let tool = echo_tool(Arc::new(|_| Ok(String::new())));
        let params = tool.parameters.clone();
        let adapter = PiToolAdapter::new(tool);
        assert_eq!(adapter.name(), "echo");
        assert_eq!(adapter.label(), "echo");
        assert_eq!(adapter.description(), "Echo the input back");
        assert_eq!(adapter.parameters(), params);
        assert_eq!(adapter.effects(), pi::tools::ToolEffects::write());
    }

    #[test]
    fn seher_tool_debug_skips_handler() {
        let tool = echo_tool(Arc::new(|_| Ok(String::new())));
        let dbg = format!("{tool:?}");
        assert!(dbg.contains("echo"), "got: {dbg}");
        assert!(!dbg.contains("handler"), "got: {dbg}");
    }
}
