//! Bridge between the new `claude-agent-sdk` crate and seher's
//! [`StreamChunk`](crate::sdk::StreamChunk) channel-based API.
//!
//! The `claude-agent-sdk` crate is the Rust port of
//! [`anthropics/claude-agent-sdk-python`]. Use the re-export
//! [`crate::claude_agent_sdk`] when you want the full async/`Stream` API; use
//! [`stream_agent`] in this module when you want to plug it into seher-cli's
//! `Receiver<StreamChunk>` consumer (`drain_to_stdout`, etc.) without writing
//! a custom adapter.
//!
//! [`anthropics/claude-agent-sdk-python`]: https://github.com/anthropics/claude-agent-sdk-python

use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use claude_agent_sdk::tool::{AgentTool, AgentToolbox};
use claude_agent_sdk::{
    ClaudeAgentOptions, ClaudeSDKError, ContentBlock, Message, PermissionMode, query,
};
use futures::StreamExt as _;

use crate::sdk::{LimitError, SeherTool, StreamChunk, is_claude_rate_limit_message};

const SEHER_TOOLBOX_NAME: &str = "seher";

/// Runner config for [`stream_agent`].
#[derive(Default)]
pub struct ClaudeAgentRunnerConfig {
    pub claude_bin: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub cwd: Option<String>,
    pub resume_session_id: Option<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// Custom in-process tools the model can call. Each [`SeherTool`] is
    /// adapted to a [`AgentTool`] and registered on a single SDK MCP toolbox
    /// (server name `"seher"`).
    pub tools: Vec<SeherTool>,
}

/// Run a prompt through `claude-agent-sdk` and surface output as
/// [`StreamChunk`]s on a dedicated thread.
///
/// Each `text` content block in an assistant message is emitted as
/// [`StreamChunk::Delta`]. Rate-limit errors are translated to
/// [`StreamChunk::Limit`]. Everything else (`tool_use`, `tool_result`,
/// `thinking`, etc.) is observed but not forwarded — callers wanting richer
/// surface should use `claude-agent-sdk` directly.
///
/// `provider_label` is purely informational; it is attached to
/// [`LimitError::provider`] when a rate-limit is detected so the CLI can
/// report which provider tripped the limit.
#[must_use]
pub fn stream_agent(
    config: ClaudeAgentRunnerConfig,
    prompt: String,
    provider_label: String,
) -> Receiver<StreamChunk> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || run_in_runtime(config, prompt, provider_label, tx));
    rx
}

fn run_in_runtime(
    config: ClaudeAgentRunnerConfig,
    prompt: String,
    provider_label: String,
    tx: Sender<StreamChunk>,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = tx.send(StreamChunk::Error("failed to build tokio runtime".into()));
        return;
    };
    rt.block_on(async move { run_async(config, prompt, provider_label, &tx).await });
}

async fn run_async(
    config: ClaudeAgentRunnerConfig,
    prompt: String,
    provider_label: String,
    tx: &Sender<StreamChunk>,
) {
    let opts = build_options(&config);
    let mut stream = match query(prompt, Some(opts), None).await {
        Ok(s) => s,
        Err(e) => {
            send_error(tx, &provider_label, &e);
            return;
        }
    };
    let mut session_id: Option<String> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(Message::Assistant(a)) => {
                if session_id.is_none()
                    && let Some(id) = a.session_id.clone()
                {
                    let _ = tx.send(StreamChunk::Session(id.clone()));
                    session_id = Some(id);
                }
                for block in a.content {
                    if let ContentBlock::Text(t) = block
                        && tx.send(StreamChunk::Delta(t.text)).is_err()
                    {
                        return;
                    }
                }
            }
            Ok(Message::Result(r)) => {
                if session_id.is_none()
                    && let Some(id) = r.session_id
                {
                    let _ = tx.send(StreamChunk::Session(id));
                }
                if r.is_error {
                    let msg = r.result.unwrap_or_else(|| r.subtype.clone());
                    if is_claude_rate_limit_message(&msg) {
                        let _ = tx.send(StreamChunk::Limit(LimitError {
                            provider: provider_label.clone(),
                            reset_at: None,
                        }));
                    } else {
                        let _ = tx.send(StreamChunk::Error(msg));
                    }
                    return;
                }
                let _ = tx.send(StreamChunk::Done(String::new()));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                send_error(tx, &provider_label, &e);
                return;
            }
        }
    }
    // Stream ended without a Result frame — emit Done anyway.
    let _ = tx.send(StreamChunk::Done(String::new()));
}

fn build_options(config: &ClaudeAgentRunnerConfig) -> ClaudeAgentOptions {
    let mut opts = ClaudeAgentOptions::new();
    opts.cli_path = config.claude_bin.as_ref().map(std::path::PathBuf::from);
    opts.model.clone_from(&config.model);
    opts.cwd = config.cwd.as_ref().map(std::path::PathBuf::from);
    opts.resume.clone_from(&config.resume_session_id);
    opts.allowed_tools.clone_from(&config.allowed_tools);
    opts.disallowed_tools.clone_from(&config.disallowed_tools);
    if let Some(s) = &config.system_prompt {
        opts.system_prompt = Some(claude_agent_sdk::types::SystemPrompt::Append(s.clone()));
    }
    opts.permission_mode = match config.permission_mode.as_deref() {
        Some("default") => Some(PermissionMode::Default),
        Some("acceptEdits") => Some(PermissionMode::AcceptEdits),
        Some("plan") => Some(PermissionMode::Plan),
        Some("bypassPermissions") | None => Some(PermissionMode::BypassPermissions),
        Some("dontAsk") => Some(PermissionMode::DontAsk),
        Some("auto") => Some(PermissionMode::Auto),
        Some(_) => None,
    };
    if !config.tools.is_empty() {
        let agent_tools: Vec<AgentTool> =
            config.tools.iter().map(seher_tool_to_agent_tool).collect();
        opts.sdk_mcp_server = Some(AgentToolbox::new(SEHER_TOOLBOX_NAME).with_tools(agent_tools));
    }
    opts
}

/// Wrap a [`SeherTool`] (sync handler) as an [`AgentTool`].
///
/// `SeherTool::handler` and `AgentTool::handler` are *the same type* — both
/// are the type alias `Arc<dyn Fn(Value) -> Result<String, String> + Send +
/// Sync>` — so the handler can be `Arc::clone`'d straight through with no
/// wrapping closure.
fn seher_tool_to_agent_tool(t: &SeherTool) -> AgentTool {
    AgentTool::new(
        t.name.clone(),
        t.description.clone(),
        t.parameters.clone(),
        t.handler.clone(),
    )
}

fn send_error(tx: &Sender<StreamChunk>, provider_label: &str, e: &ClaudeSDKError) {
    let msg = e.to_string();
    if is_claude_rate_limit_message(&msg) {
        let _ = tx.send(StreamChunk::Limit(LimitError {
            provider: provider_label.to_string(),
            reset_at: None,
        }));
    } else {
        let _ = tx.send(StreamChunk::Error(msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rate-limit phrase detection is covered by
    // `sdk::errors::is_claude_rate_limit_message` tests.

    #[test]
    fn build_options_translates_permission_modes() {
        let cfg = ClaudeAgentRunnerConfig {
            permission_mode: Some("plan".into()),
            ..Default::default()
        };
        let opts = build_options(&cfg);
        assert!(matches!(opts.permission_mode, Some(PermissionMode::Plan)));

        let cfg = ClaudeAgentRunnerConfig::default();
        let opts = build_options(&cfg);
        assert!(matches!(
            opts.permission_mode,
            Some(PermissionMode::BypassPermissions)
        ));
    }

    #[test]
    fn build_options_carries_model_and_cwd() {
        let cfg = ClaudeAgentRunnerConfig {
            model: Some("claude-sonnet-4-6".into()),
            cwd: Some("/tmp".into()),
            allowed_tools: vec!["Read".into()],
            ..Default::default()
        };
        let opts = build_options(&cfg);
        assert_eq!(opts.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(opts.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(opts.allowed_tools, vec!["Read".to_string()]);
    }
}
