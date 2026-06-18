//! Tiny smoke example: print every message until the CLI exits.
//!
//! Requires `claude` to be on `$PATH` (or pass `--cli-path` via env).
//! Run:
//! ```bash
//! cargo run -p seher-claude-agent-sdk --example quickstart -- "What is 2+2?"
//! ```

use std::env;

use claude_agent_sdk::{ClaudeAgentOptions, Message, query};
use futures::StreamExt as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = env::args().nth(1).unwrap_or_else(|| "Hello!".into());
    let opts = ClaudeAgentOptions {
        permission_mode: Some(claude_agent_sdk::PermissionMode::BypassPermissions),
        ..Default::default()
    };

    let mut stream = query(prompt, Some(opts), None).await?;
    while let Some(msg) = stream.next().await {
        match msg? {
            Message::Assistant(a) => {
                for block in a.content {
                    if let claude_agent_sdk::ContentBlock::Text(t) = block {
                        println!("{}", t.text);
                    }
                }
            }
            Message::Result(r) => {
                eprintln!("[result] subtype={} cost={:?}", r.subtype, r.total_cost_usd);
            }
            _ => {}
        }
    }
    Ok(())
}
