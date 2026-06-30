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

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use claude_agent_sdk::internal::client::user_message_frame;
use claude_agent_sdk::tool::{AgentTool, AgentToolbox};
use claude_agent_sdk::transport::{SubprocessCliTransport, Transport};
use claude_agent_sdk::{ClaudeAgentOptions, ContentBlock, Message, PermissionMode};
use futures::StreamExt as _;

use crate::sdk::{EffortLevel, LimitError, SeherTool, StreamChunk, is_claude_rate_limit_message};

const SEHER_TOOLBOX_NAME: &str = "seher";

/// Number of trailing stderr lines retained for rate-limit classification.
/// 64 lines is more than enough for a CLI error tail; capping prevents
/// unbounded growth if the CLI starts logging copiously to stderr.
const STDERR_TAIL_LINES: usize = 64;

/// Runner config for [`stream_agent`].
#[derive(Default)]
pub struct ClaudeAgentRunnerConfig {
    pub claude_bin: Option<String>,
    pub model: Option<String>,
    pub effort: Option<EffortLevel>,
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
    pub env: std::collections::HashMap<String, String>,
}

/// Run a prompt through `claude-agent-sdk` and surface output as
/// [`StreamChunk`]s on a dedicated thread.
///
/// Each `text` content block in an assistant message is emitted as
/// [`StreamChunk::Delta`]. Rate-limit errors are translated to
/// [`StreamChunk::Limit`]. Everything else (`tool_use`, `tool_result`,
/// `thinking`, etc.) is observed but not forwarded -- callers wanting richer
/// surface should use `claude-agent-sdk` directly.
///
/// Rate-limit detection consults both the SDK error message and the trailing
/// stderr captured from the `claude` CLI process. The CLI sometimes truncates
/// stream-json output mid-frame on rate-limit (the next read surfaces a
/// `ClaudeSDKError::JsonDecode` whose 100-char snippet contains no limit
/// keyword); in that case the actual `rate limit` / `429` text lands on
/// stderr, so we mirror it into [`LimitError`] via the trailing-line buffer.
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

/// Terminal outcome of one CLI run. Delta/Session chunks are streamed out
/// inline while the stream is being read; the terminal verdict is delayed
/// until after stderr has been fully drained so it can consult the trailing
/// lines.
enum Terminal {
    /// `Result` frame arrived with `is_error: true`. `msg` is the CLI-reported
    /// error text (or the subtype as a fallback).
    ResultError(String),
    /// The message stream surfaced a transport/parse error.
    StreamError(String),
    /// Stream ended cleanly (Result with `is_error=false`, or no Result
    /// frame at all).
    Ok,
}

/// Cap on how long we wait for the stderr drain task to consume residual
/// lines after the transport closes. The child has already exited at this
/// point so 2s is plenty; the timeout exists only to prevent a runaway task
/// from blocking the runtime.
const STDERR_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn run_async(
    config: ClaudeAgentRunnerConfig,
    prompt: String,
    provider_label: String,
    tx: &Sender<StreamChunk>,
) {
    let opts = build_options(&config);
    // When custom tools are registered, the CLI initiates an MCP `initialize`
    // handshake over stdout/stdin shortly after start; in `--print` mode
    // (`one_shot`) stdin isn't open for input frames and the CLI marks the
    // SDK MCP server as `failed`, so the tools never reach the model. Use
    // streaming mode and push the prompt as a user frame ourselves; the
    // strong write_tx the demux holds when a control handler is registered
    // (see claude-agent-sdk subprocess_cli.rs) keeps the control_response
    // channel alive across `end_input()`.
    let has_tools = !config.tools.is_empty();
    // Pre-build the user frame so the no-tools branch can move `prompt` into
    // the one_shot transport without an extra clone.
    let user_frame = has_tools.then(|| user_message_frame(&prompt, "default"));
    let mut transport = if has_tools {
        SubprocessCliTransport::streaming(opts)
    } else {
        SubprocessCliTransport::one_shot(opts, prompt)
    };
    if let Err(e) = transport.connect().await {
        // No stderr yet -- fall back to the SDK message alone.
        send_error_with_stderr(tx, &provider_label, &e.to_string(), &[]);
        return;
    }

    // Drain stderr into a small ring buffer so we can inspect it after a
    // transport error or premature stream end. Without this, the CLI's
    // rate-limit messages on stderr are silently dropped. We retain the
    // JoinHandle to await drain completion after the child exits -- see the
    // `await_stderr_drain` call below for the race-avoidance rationale.
    // Spawned before the streaming-mode write so that a write failure
    // (typically caused by the child dying mid-handshake) still has access to
    // any rate-limit / auth errors the CLI emitted on stderr.
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let drain_handle: Option<tokio::task::JoinHandle<()>> =
        transport.take_stderr_rx().map(|mut rx_stderr| {
            let buf = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                while let Some(line) = rx_stderr.recv().await {
                    push_stderr_line(&buf, line);
                }
            })
        });

    if let Some(frame) = user_frame {
        // Match the wire format and session-id convention used by the SDK's
        // own `query()` (see claude-agent-sdk query.rs) so changes there stay
        // in sync. The strong write_tx the demux holds for control handlers
        // keeps MCP `initialize` answerable across `end_input()`.
        if let Err(e) = transport.write(&frame).await {
            let tail = snapshot_stderr_tail(&stderr_tail);
            send_error_with_stderr(tx, &provider_label, &e.to_string(), &tail);
            return;
        }
        if let Err(e) = transport.end_input().await {
            let tail = snapshot_stderr_tail(&stderr_tail);
            send_error_with_stderr(tx, &provider_label, &e.to_string(), &tail);
            return;
        }
    }

    let mut stream = transport
        .take_message_stream()
        .map(|item| item.and_then(Message::from_frame));

    let mut session_id: Option<String> = None;
    let mut terminal = Terminal::Ok;
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
                    terminal = Terminal::ResultError(r.result.unwrap_or_else(|| r.subtype.clone()));
                }
                break;
            }
            Ok(_) => {}
            Err(e) => {
                terminal = Terminal::StreamError(e.to_string());
                break;
            }
        }
    }
    // Drop the message stream to release its receiver, then close the
    // transport (waits up to 5s for the child to exit). Only *after* the
    // child is gone is stderr guaranteed to be at EOF -- so we then await the
    // drain task, which exits once its mpsc upstream closes. Without this,
    // snapshotting too early would miss the rate-limit line that arrived in
    // the same scheduling tick as the stdout error.
    drop(stream);
    let _ = transport.close().await;
    await_stderr_drain(drain_handle).await;
    let tail = snapshot_stderr_tail(&stderr_tail);

    match terminal {
        Terminal::ResultError(msg) | Terminal::StreamError(msg) => {
            send_error_with_stderr(tx, &provider_label, &msg, &tail);
        }
        Terminal::Ok => {
            // Stream ended cleanly. Even so, the CLI sometimes exits without
            // a Result frame when it dies mid-stream on a rate-limit; check
            // stderr as a last line of defense.
            if let Some(limit) = limit_from_stderr(&tail, &provider_label) {
                let _ = tx.send(StreamChunk::Limit(limit));
            } else {
                let _ = tx.send(StreamChunk::Done(String::new()));
            }
        }
    }
}

/// Wait for the stderr drain task to finish processing residual lines.
///
/// The task exits naturally when its upstream mpsc sender (held by the SDK's
/// `read_stderr_loop`) is dropped, which happens after the child's stderr
/// pipe EOFs. We cap the wait at [`STDERR_DRAIN_TIMEOUT`] so a runaway task
/// can't block the runtime; in practice the child has already exited by the
/// time we call this, so completion is near-instant.
async fn await_stderr_drain(handle: Option<tokio::task::JoinHandle<()>>) {
    let Some(h) = handle else { return };
    let _ = tokio::time::timeout(STDERR_DRAIN_TIMEOUT, h).await;
}

fn push_stderr_line(buf: &Arc<Mutex<VecDeque<String>>>, line: String) {
    // PoisonError carries the inner guard; recover instead of giving up the
    // entire stderr tail just because some other task panicked.
    let mut guard = match buf.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.len() >= STDERR_TAIL_LINES {
        guard.pop_front();
    }
    guard.push_back(line);
}

fn snapshot_stderr_tail(buf: &Arc<Mutex<VecDeque<String>>>) -> Vec<String> {
    match buf.lock() {
        Ok(g) => g.iter().cloned().collect(),
        Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
    }
}

/// Inspect collected stderr lines for rate-limit signals.
///
/// Returns `Some(LimitError)` when any line matches
/// [`is_claude_rate_limit_message`]. `reset_at` is left `None`; the upstream
/// CLI does not surface a machine-readable reset time on stderr, so retry
/// timing is delegated to the resolver.
fn limit_from_stderr(tail: &[String], provider_label: &str) -> Option<LimitError> {
    let joined = tail.join("\n");
    if !is_claude_rate_limit_message(&joined) {
        return None;
    }
    Some(LimitError {
        provider: provider_label.to_string(),
        reset_at: None,
    })
}

fn build_options(config: &ClaudeAgentRunnerConfig) -> ClaudeAgentOptions {
    let mut opts = ClaudeAgentOptions::new();
    opts.cli_path = config.claude_bin.as_ref().map(std::path::PathBuf::from);
    opts.model.clone_from(&config.model);
    opts.effort = config.effort.map(EffortLevel::as_str).map(String::from);
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
    opts.env.clone_from(&config.env);
    opts
}

/// Wrap a [`SeherTool`] (sync handler) as an [`AgentTool`].
///
/// `SeherTool::handler` and `AgentTool::handler` are *the same type* -- both
/// are the type alias `Arc<dyn Fn(Value) -> Result<String, String> + Send +
/// Sync>` -- so the handler can be `Arc::clone`'d straight through with no
/// wrapping closure.
fn seher_tool_to_agent_tool(t: &SeherTool) -> AgentTool {
    AgentTool::new(
        t.name.clone(),
        t.description.clone(),
        t.parameters.clone(),
        t.handler.clone(),
    )
}

/// Classify `msg` together with the collected stderr tail.
///
/// The SDK error message alone is often insufficient: when the CLI is
/// rate-limited it commonly emits a truncated assistant frame that we surface
/// as `ClaudeSDKError::JsonDecode`, whose 100-char snippet doesn't include
/// the rate-limit phrase. The actual phrase lives on stderr, so we feed both
/// into [`is_claude_rate_limit_message`] before deciding Limit vs. Error.
fn send_error_with_stderr(
    tx: &Sender<StreamChunk>,
    provider_label: &str,
    msg: &str,
    stderr_tail: &[String],
) {
    let stderr_joined = stderr_tail.join("\n");
    let combined = if stderr_joined.is_empty() {
        msg.to_string()
    } else {
        format!("{msg}\n{stderr_joined}")
    };
    if is_claude_rate_limit_message(&combined) {
        let _ = tx.send(StreamChunk::Limit(LimitError {
            provider: provider_label.to_string(),
            reset_at: None,
        }));
    } else {
        let _ = tx.send(StreamChunk::Error(msg.to_string()));
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
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

    #[test]
    fn build_options_carries_effort() {
        let cfg = ClaudeAgentRunnerConfig {
            effort: Some(EffortLevel::XHigh),
            ..Default::default()
        };
        let opts = build_options(&cfg);
        assert_eq!(opts.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn build_options_effort_none_when_unset() {
        let cfg = ClaudeAgentRunnerConfig::default();
        let opts = build_options(&cfg);
        assert_eq!(opts.effort, None);
    }

    // -- send_error_with_stderr classification -------------------------------

    fn drain(rx: &Receiver<StreamChunk>) -> StreamChunk {
        rx.recv().expect("a chunk was sent")
    }

    #[test]
    fn send_error_with_stderr_emits_limit_when_stderr_has_phrase() {
        // The classic rate-limit-mid-frame case: the SDK surfaces a truncated
        // JsonDecode error whose snippet has no rate-limit phrase, but stderr
        // contains "rate limit exceeded".
        let (tx, rx) = std::sync::mpsc::channel();
        let msg = r#"failed to decode JSON: {"type":"assistant","message":{"model":"claude-sonnet-4-6","id":"msg_xxx","type"#;
        let tail = vec!["API Error: 429 rate limit exceeded".to_string()];
        send_error_with_stderr(&tx, "anthropic", msg, &tail);
        match drain(&rx) {
            StreamChunk::Limit(e) => assert_eq!(e.provider, "anthropic"),
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    #[test]
    fn send_error_with_stderr_emits_error_when_no_rate_limit_signal() {
        let (tx, rx) = std::sync::mpsc::channel();
        send_error_with_stderr(&tx, "anthropic", "boom", &["unrelated noise".into()]);
        match drain(&rx) {
            StreamChunk::Error(m) => assert_eq!(m, "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn send_error_with_stderr_classifies_from_msg_alone() {
        // Backward-compatible path: stderr empty, message contains the phrase.
        let (tx, rx) = std::sync::mpsc::channel();
        send_error_with_stderr(&tx, "anthropic", "Too Many Requests", &[]);
        assert!(matches!(drain(&rx), StreamChunk::Limit(_)));
    }

    // -- limit_from_stderr (stream end without Result frame) -----------------

    #[test]
    fn limit_from_stderr_detects_phrase_in_any_line() {
        let tail = vec![
            "INFO: starting request".into(),
            "Error: usage limit reached".into(),
        ];
        let limit = limit_from_stderr(&tail, "anthropic").expect("limit detected");
        assert_eq!(limit.provider, "anthropic");
    }

    #[test]
    fn limit_from_stderr_returns_none_without_signal() {
        let tail = vec!["nothing to see here".into()];
        assert!(limit_from_stderr(&tail, "anthropic").is_none());
    }

    // -- push_stderr_line / snapshot caps at STDERR_TAIL_LINES ---------------

    #[test]
    fn stderr_ring_buffer_caps_at_capacity() {
        let buf: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        for i in 0..(STDERR_TAIL_LINES + 5) {
            push_stderr_line(&buf, format!("line {i}"));
        }
        let tail = snapshot_stderr_tail(&buf);
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        // Oldest 5 lines must have been evicted.
        assert_eq!(tail[0], format!("line 5"));
        assert_eq!(
            tail[STDERR_TAIL_LINES - 1],
            format!("line {}", STDERR_TAIL_LINES + 4)
        );
    }
}
