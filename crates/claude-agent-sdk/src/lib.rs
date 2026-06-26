//! Rust port of [`anthropics/claude-agent-sdk-python`].
//!
//! Drive the `claude` CLI as an agent runtime: send prompts, stream messages
//! back, and (optionally) keep the session open for follow-up turns.
//!
//! Two entrypoints, matching the Python SDK:
//!
//! * [`query`] -- one-shot, fire-and-forget. Yields messages until the CLI exits.
//! * [`ClaudeSDKClient`] -- stateful client with `connect` / `query` /
//!   `receive_messages` / `end_input` / `disconnect`.
//!
//! Python-SDK control commands such as `interrupt`, `set_model`, and the
//! `hooks` / `can_use_tool` callbacks are not implemented yet. In-process MCP
//! tools (the [`tool`] module's `AgentTool` / `AgentToolbox`) and the
//! `control_request` plumbing they need *are* wired up.
//!
//! Both entrypoints are powered by a [`Transport`] trait. The default
//! implementation [`SubprocessCliTransport`] spawns the `claude` CLI with
//! `--output-format stream-json --input-format stream-json` and pipes JSON
//! frames over stdin/stdout.
//!
//! [`anthropics/claude-agent-sdk-python`]: https://github.com/anthropics/claude-agent-sdk-python

pub mod client;
pub mod control;
pub mod errors;
pub mod internal;
pub mod query;
pub mod tool;
pub mod transport;
pub mod types;

pub use client::ClaudeSDKClient;
pub use errors::{ClaudeSDKError, Result};
pub use query::query;
pub use transport::{SubprocessCliTransport, Transport};
pub use types::{
    AssistantMessage, ClaudeAgentOptions, ContentBlock, McpServerConfig, Message, PermissionMode,
    ResultMessage, StreamEvent, SystemMessage, TextBlock, ThinkingBlock, ToolResultBlock,
    ToolUseBlock, UserMessage,
};
