//! Register an in-process tool with the `claude` CLI and let the model call
//! it.
//!
//! Run:
//! ```bash
//! cargo run -p claude-agent-sdk --example with_tools -- \
//!   "Use the add tool to compute 17 + 25."
//! ```

use std::env;
use std::sync::Arc;

use claude_agent_sdk::tool::{AgentTool, AgentToolbox};
use claude_agent_sdk::{ClaudeAgentOptions, ContentBlock, Message, PermissionMode, query};
use futures::StreamExt as _;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = env::args()
        .nth(1)
        .unwrap_or_else(|| "Use the add tool to compute 17 + 25.".into());

    let add = AgentTool::new(
        "add",
        "Add two integers and return the sum.",
        json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"}
            },
            "required": ["a", "b"],
        }),
        Arc::new(|input| {
            let a = input
                .get("a")
                .and_then(serde_json::Value::as_i64)
                .ok_or("missing 'a'")?;
            let b = input
                .get("b")
                .and_then(serde_json::Value::as_i64)
                .ok_or("missing 'b'")?;
            Ok(format!("{}", a + b))
        }),
    );
    let toolbox = AgentToolbox::new("demo").with_tools(vec![add]);

    let opts = ClaudeAgentOptions {
        permission_mode: Some(PermissionMode::BypassPermissions),
        sdk_mcp_server: Some(toolbox),
        // Explicitly allow the toolbox's tools so they aren't gated by
        // permission prompts. The CLI exposes SDK tools as
        // `mcp__<server>__<tool>`.
        allowed_tools: vec!["mcp__demo__add".into()],
        ..Default::default()
    };

    let mut stream = query(prompt, Some(opts), None).await?;
    while let Some(msg) = stream.next().await {
        match msg? {
            Message::Assistant(a) => {
                for block in a.content {
                    match block {
                        ContentBlock::Text(t) => println!("{}", t.text),
                        ContentBlock::ToolUse(tu) => {
                            eprintln!("[tool_use] {} ({})", tu.name, tu.input);
                        }
                        _ => {}
                    }
                }
            }
            Message::Result(r) => {
                eprintln!(
                    "[result] subtype={} is_error={} cost={:?}",
                    r.subtype, r.is_error, r.total_cost_usd
                );
            }
            _ => {}
        }
    }
    Ok(())
}
