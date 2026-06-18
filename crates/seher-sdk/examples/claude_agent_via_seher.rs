//! Example: drive `claude-agent-sdk` (the Rust port of
//! `anthropics/claude-agent-sdk-python`) through the seher re-export, and
//! also through the seher `StreamChunk` bridge.
//!
//! Run with a prompt argument:
//! ```bash
//! cargo run -p seher-sdk --example claude_agent_via_seher -- "Hello"
//! ```

use std::env;

use futures::StreamExt as _;
use seher::claude_agent::{ClaudeAgentRunnerConfig, stream_agent};
use seher::claude_agent_sdk::{ClaudeAgentOptions, Message, PermissionMode, query};
use seher::sdk::StreamChunk;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = env::args().nth(1).unwrap_or_else(|| "What is 2+2?".into());

    // -- Path 1: direct SDK use ------------------------------------------
    println!("--- direct claude-agent-sdk ---");
    let opts = ClaudeAgentOptions {
        permission_mode: Some(PermissionMode::BypassPermissions),
        ..Default::default()
    };
    let mut stream = query(prompt.clone(), Some(opts), None).await?;
    while let Some(msg) = stream.next().await {
        match msg? {
            Message::Assistant(a) => {
                for block in a.content {
                    if let seher::claude_agent_sdk::ContentBlock::Text(t) = block {
                        print!("{}", t.text);
                    }
                }
                println!();
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

    // -- Path 2: StreamChunk bridge (for `seher` consumers) --------------
    println!("--- via seher StreamChunk bridge ---");
    let rx = stream_agent(
        ClaudeAgentRunnerConfig {
            permission_mode: Some("bypassPermissions".into()),
            ..Default::default()
        },
        prompt,
        "claude".to_string(),
    );
    while let Ok(chunk) = rx.recv() {
        match chunk {
            StreamChunk::Delta(d) => print!("{d}"),
            StreamChunk::Session(id) => eprintln!("[session] {id}"),
            StreamChunk::Done(_) => {
                println!();
                break;
            }
            StreamChunk::Limit(e) => {
                eprintln!("[rate-limit] provider={}", e.provider);
                break;
            }
            StreamChunk::Error(m) => {
                eprintln!("[error] {m}");
                break;
            }
        }
    }
    Ok(())
}
