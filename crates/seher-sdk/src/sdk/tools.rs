//! In-process tools forwarded to pi via [`pi::sdk::ToolFactory`].
//!
//! Define a tool with [`SeherTool`], wrap them with [`make_factory`], and pass
//! the resulting `Arc<dyn ToolFactory>` into [`crate::sdk::PiRunnerOptions::tool_factory`].

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use pi::sdk::{
    Config as PiConfig, ContentBlock, TextContent, Tool, ToolFactory, ToolOutput, ToolRegistry,
    ToolUpdate, default_tool_registry,
};

/// Synchronous tool handler. Takes parsed JSON args, returns the tool's text
/// output on success or an error message on failure (surfaced to the model as
/// `is_error = true`).
pub type ToolHandler =
    Arc<dyn Fn(serde_json::Value) -> Result<String, String> + Send + Sync + 'static>;

/// User-facing in-process tool definition. Mirrors `seher-ts/packages/sdk/src/sdk/tools.ts`.
#[derive(Clone)]
pub struct SeherTool {
    /// Tool name (must be unique within a session).
    pub name: String,
    /// One-line description shown to the model.
    pub description: String,
    /// JSON Schema describing the tool's input parameters.
    pub parameters: serde_json::Value,
    /// Handler invoked when the model calls this tool.
    pub handler: ToolHandler,
}

struct SeherToolAdapter {
    tool: SeherTool,
}

#[async_trait]
impl Tool for SeherToolAdapter {
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

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> pi::sdk::Result<ToolOutput> {
        let (text, is_error) = match (self.tool.handler)(input) {
            Ok(t) => (t, false),
            Err(e) => (e, true),
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error,
        })
    }
}

/// `ToolFactory` impl that layers user [`SeherTool`]s on top of pi's default
/// built-in tool set.
pub struct SeherToolFactory {
    tools: Vec<SeherTool>,
}

impl ToolFactory for SeherToolFactory {
    fn create_tool_registry(
        &self,
        enabled: &[&str],
        cwd: &Path,
        config: &PiConfig,
    ) -> ToolRegistry {
        let mut reg = default_tool_registry(enabled, cwd, config);
        for t in &self.tools {
            reg.push(Box::new(SeherToolAdapter { tool: t.clone() }));
        }
        reg
    }
}

/// Build an `Arc<dyn ToolFactory>` from a list of [`SeherTool`]s, ready to
/// assign to [`crate::sdk::PiRunnerOptions::tool_factory`].
#[must_use]
pub fn make_factory(tools: Vec<SeherTool>) -> Arc<dyn ToolFactory> {
    Arc::new(SeherToolFactory { tools })
}
