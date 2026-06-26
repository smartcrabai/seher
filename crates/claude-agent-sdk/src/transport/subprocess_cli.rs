//! Spawn the `claude` CLI and pipe stream-json frames over stdin/stdout.
//!
//! This is the Rust counterpart of
//! `claude_agent_sdk._internal.transport.subprocess_cli.SubprocessCLITransport`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::control::{ControlHandler, ControlRequest, ControlResponse};
use crate::errors::{ClaudeSDKError, Result};
use crate::transport::Transport;
use crate::types::{ClaudeAgentOptions, SystemPrompt};

const DEFAULT_MAX_BUFFER_SIZE: usize = 1024 * 1024;
const SDK_ENTRYPOINT: &str = "sdk-rust";

/// Subprocess-backed transport.
///
/// Spawns `claude --output-format stream-json --input-format stream-json --verbose`
/// plus whatever flags [`ClaudeAgentOptions`] translates into, and exposes the
/// CLI's stdout as a stream of JSON values.
///
/// `prompt` controls input:
/// - `Some(text)` -> string mode: `--print "<text>"` (no stdin streaming).
/// - `None` -> streaming mode: callers push JSON frames via
///   [`Transport::write`].
pub struct SubprocessCliTransport {
    options: ClaudeAgentOptions,
    prompt: Option<String>,
    cli_path: Option<PathBuf>,
    child: Option<Child>,
    /// Writer task channel. All stdin writes -- both caller-initiated
    /// (`write()`) and `control_response`s emitted by the demux loop -- go
    /// through here so we never have two writers racing on the same pipe.
    write_tx: Option<mpsc::Sender<String>>,
    stdout_rx: Option<mpsc::Receiver<Result<serde_json::Value>>>,
    /// Optional stderr collector; the receiver is taken when the caller asks
    /// for it via [`Self::take_stderr_rx`] (otherwise it's dropped at close).
    stderr_rx: Option<mpsc::Receiver<String>>,
    /// Handler invoked from the demux loop for every `control_request` frame.
    /// When `None`, requests are answered with `ControlResponse::Error` so
    /// the CLI doesn't hang waiting forever.
    control_handler: Option<Arc<dyn ControlHandler>>,
    closed: bool,
}

impl SubprocessCliTransport {
    /// Build a transport for a streaming session (callers `write()` frames).
    #[must_use]
    pub fn streaming(options: ClaudeAgentOptions) -> Self {
        Self::with_prompt(options, None)
    }

    /// Build a transport that fires `--print "<prompt>"` and reads results.
    #[must_use]
    pub fn one_shot(options: ClaudeAgentOptions, prompt: String) -> Self {
        Self::with_prompt(options, Some(prompt))
    }

    fn with_prompt(options: ClaudeAgentOptions, prompt: Option<String>) -> Self {
        // If the caller supplied an `sdk_mcp_server` toolbox, auto-register a
        // matching control handler so `mcp_message` requests get served
        // without the caller having to wire anything else.
        let control_handler: Option<Arc<dyn ControlHandler>> =
            options.sdk_mcp_server.as_ref().map(|tb| {
                Arc::new(crate::tool::ToolboxControlHandler::new(tb.clone()))
                    as Arc<dyn ControlHandler>
            });
        Self {
            cli_path: options.cli_path.clone(),
            options,
            prompt,
            child: None,
            write_tx: None,
            stdout_rx: None,
            stderr_rx: None,
            control_handler,
            closed: false,
        }
    }

    /// Register a [`ControlHandler`]. Must be called before
    /// [`Self::connect`]; switching the handler after the child is spawned
    /// has no effect.
    ///
    /// **Overrides** any handler that was auto-registered from
    /// `options.sdk_mcp_server`. If you want both an in-process toolbox
    /// *and* extra handling (hooks, `can_use_tool`, ...), build your own
    /// composite handler and register it here -- do not rely on stacking.
    #[must_use]
    pub fn with_control_handler(mut self, handler: Arc<dyn ControlHandler>) -> Self {
        self.control_handler = Some(handler);
        self
    }

    /// Set the control handler in place. Equivalent to
    /// [`Self::with_control_handler`] for `&mut self` callers, with the same
    /// override semantics.
    pub fn set_control_handler(&mut self, handler: Arc<dyn ControlHandler>) {
        self.control_handler = Some(handler);
    }

    /// Take ownership of the stderr line channel. After [`Transport::connect`]
    /// each `claude` stderr line is forwarded here; if you don't drain it,
    /// lines are silently dropped (bounded buffer).
    #[must_use]
    pub fn take_stderr_rx(&mut self) -> Option<mpsc::Receiver<String>> {
        self.stderr_rx.take()
    }

    /// Compute the binary to invoke. Honors `options.cli_path`, then `$PATH`
    /// (resolved by the OS at spawn time).
    fn resolve_bin(&self) -> String {
        self.cli_path.as_ref().map_or_else(
            || "claude".to_string(),
            |p| p.to_string_lossy().into_owned(),
        )
    }

    /// Translate [`ClaudeAgentOptions`] into CLI arguments. Public for tests
    /// and for callers that want to assemble their own [`Command`].
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "one branch per CLI flag -- splitting hurts readability more than it helps"
    )]
    pub fn build_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec!["--output-format".into(), "stream-json".into()];
        args.extend(["--verbose".into()]);

        if self.prompt.is_some() {
            // string mode -- caller passes `--print "<prompt>"`
        } else {
            args.extend(["--input-format".into(), "stream-json".into()]);
        }

        match &self.options.system_prompt {
            Some(SystemPrompt::Inline(s) | SystemPrompt::Preset(s)) => {
                args.push("--system-prompt".into());
                args.push(s.clone());
            }
            Some(SystemPrompt::File(p)) => {
                args.push("--system-prompt-file".into());
                args.push(p.to_string_lossy().into_owned());
            }
            Some(SystemPrompt::Append(s)) => {
                args.push("--append-system-prompt".into());
                args.push(s.clone());
            }
            None => {}
        }

        if !self.options.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.push(self.options.allowed_tools.join(","));
        }
        if !self.options.disallowed_tools.is_empty() {
            args.push("--disallowedTools".into());
            args.push(self.options.disallowed_tools.join(","));
        }
        if let Some(n) = self.options.max_turns {
            args.push("--max-turns".into());
            args.push(n.to_string());
        }
        if let Some(b) = self.options.max_budget_usd {
            args.push("--max-budget-usd".into());
            args.push(b.to_string());
        }
        if let Some(m) = &self.options.model {
            args.push("--model".into());
            args.push(m.clone());
        }
        if let Some(m) = &self.options.fallback_model {
            args.push("--fallback-model".into());
            args.push(m.clone());
        }
        if !self.options.betas.is_empty() {
            args.push("--betas".into());
            args.push(self.options.betas.join(","));
        }
        if let Some(name) = &self.options.permission_prompt_tool_name {
            args.push("--permission-prompt-tool".into());
            args.push(name.clone());
        }
        if let Some(mode) = self.options.permission_mode {
            args.push("--permission-mode".into());
            args.push(mode.as_str().into());
        }
        if self.options.continue_conversation {
            args.push("--continue".into());
        }
        if let Some(id) = &self.options.resume {
            args.push("--resume".into());
            args.push(id.clone());
        }
        if let Some(id) = &self.options.session_id {
            args.push("--session-id".into());
            args.push(id.clone());
        }
        if self.options.fork_session {
            args.push("--fork-session".into());
        }
        if let Some(s) = &self.options.settings {
            args.push("--settings".into());
            args.push(s.clone());
        }
        for dir in &self.options.add_dirs {
            args.push("--add-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        let mut mcp_map: serde_json::Map<String, serde_json::Value> = self
            .options
            .mcp_servers
            .iter()
            .map(|(k, v)| (k.clone(), v.0.clone()))
            .collect();
        if let Some(tb) = &self.options.sdk_mcp_server {
            // Mirrors the Python SDK: in-process toolboxes are advertised as
            // `{"type": "sdk", "name": "<name>"}` -- the actual handler runs
            // here, not in the CLI.
            mcp_map.insert(
                tb.name.clone(),
                serde_json::json!({"type": "sdk", "name": tb.name}),
            );
        }
        if !mcp_map.is_empty() {
            let json = serde_json::json!({"mcpServers": mcp_map});
            args.push("--mcp-config".into());
            args.push(json.to_string());
        }
        if self.options.strict_mcp_config {
            args.push("--strict-mcp-config".into());
        }
        if self.options.include_partial_messages {
            args.push("--include-partial-messages".into());
        }
        if self.options.include_hook_events {
            args.push("--include-hook-events".into());
        }
        if !self.options.setting_sources.is_empty() {
            args.push(format!(
                "--setting-sources={}",
                self.options.setting_sources.join(",")
            ));
        }
        for (flag, value) in &self.options.extra_args {
            args.push(flag.clone());
            if let Some(v) = value {
                args.push(v.clone());
            }
        }

        if let Some(text) = &self.prompt {
            args.push("--print".into());
            args.push(text.clone());
        }

        args
    }
}

#[async_trait]
impl Transport for SubprocessCliTransport {
    async fn connect(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let bin = self.resolve_bin();
        let args = self.build_args();
        let mut cmd = Command::new(&bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("CLAUDE_CODE_ENTRYPOINT", SDK_ENTRYPOINT)
            .env("CLAUDE_AGENT_SDK_VERSION", env!("CARGO_PKG_VERSION"))
            .env_remove("CLAUDECODE");
        if let Some(cwd) = &self.options.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &self.options.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClaudeSDKError::CliNotFound {
                    path: Some(bin.clone()),
                }
            } else {
                ClaudeSDKError::Connection(format!("failed to spawn {bin}: {e}"))
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClaudeSDKError::connection("missing stdin from claude process"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClaudeSDKError::connection("missing stdout from claude process"))?;
        let stderr = child.stderr.take();

        let max_buf = self
            .options
            .max_buffer_size
            .unwrap_or(DEFAULT_MAX_BUFFER_SIZE);

        // Writer task: owns stdin. Both user writes and control_response
        // writes funnel through the same channel.
        let (write_tx, write_rx) = mpsc::channel::<String>(64);
        tokio::spawn(write_stdin_loop(stdin, write_rx));

        let (msg_tx, msg_rx) = mpsc::channel::<Result<serde_json::Value>>(64);
        // Reader+demux task: pulls JSON frames off stdout, splits
        // control_request out to the handler, forwards the rest to msg_tx.
        //
        // How it holds the stdin writer depends on whether a control handler
        // is registered:
        //
        // * No handler -> a *weak* sender. There is no control_response to
        //   write, so dropping the caller's `write_tx` (e.g. from
        //   `end_input`) should close the writer channel and shut down stdin
        //   immediately. A strong clone would keep the writer alive until the
        //   reader exits, defeating `end_input`.
        //
        // * Handler present (e.g. an `sdk_mcp_server` toolbox) -> a *strong*
        //   sender. The CLI sends its `mcp_message: initialize` handshake
        //   *after* the caller has typically already called `end_input()`,
        //   which drops the transport's strong `write_tx`. With only a weak
        //   sender the demux can't upgrade it, so the initialize response is
        //   silently dropped and the CLI marks the server `failed`. Keeping a
        //   strong sender lets us answer the handshake; it's dropped when the
        //   stdout reader hits EOF (after the CLI's `Result` frame), so the
        //   writer channel still closes naturally.
        let writer_for_demux = if self.control_handler.is_some() {
            DemuxSender::Strong(write_tx.clone())
        } else {
            DemuxSender::Weak(write_tx.downgrade())
        };
        tokio::spawn(read_stdout_loop(
            stdout,
            msg_tx,
            max_buf,
            self.control_handler.clone(),
            writer_for_demux,
        ));

        let (etx, erx) = mpsc::channel::<String>(64);
        if let Some(stderr) = stderr {
            tokio::spawn(read_stderr_loop(stderr, etx));
        }

        self.child = Some(child);
        self.write_tx = Some(write_tx);
        self.stdout_rx = Some(msg_rx);
        self.stderr_rx = Some(erx);
        self.closed = false;
        Ok(())
    }

    async fn write(&mut self, line: &str) -> Result<()> {
        let tx = self
            .write_tx
            .as_ref()
            .ok_or_else(|| ClaudeSDKError::connection("transport not connected"))?;
        let mut owned = line.to_string();
        if !owned.ends_with('\n') {
            owned.push('\n');
        }
        tx.send(owned)
            .await
            .map_err(|_| ClaudeSDKError::connection("writer task closed"))?;
        Ok(())
    }

    fn take_message_stream(&mut self) -> BoxStream<'static, Result<serde_json::Value>> {
        // Move the receiver out so the returned stream owns it; if connect
        // hasn't run yet we yield an immediate error and stop.
        let rx = self.stdout_rx.take();
        match rx {
            Some(rx) => {
                let s = futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                });
                s.boxed()
            }
            None => stream::once(async {
                Err(ClaudeSDKError::connection(
                    "take_message_stream called before connect or already consumed",
                ))
            })
            .boxed(),
        }
    }

    async fn end_input(&mut self) -> Result<()> {
        // Dropping the sender closes the channel; the writer task exits and
        // shuts down stdin.
        self.write_tx = None;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.end_input().await?;
        if let Some(mut child) = self.child.take() {
            // Try graceful exit first, then escalate.
            let waited =
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            if !matches!(waited, Ok(Ok(_))) {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        self.write_tx = None;
        self.stdout_rx = None;
        self.stderr_rx = None;
        Ok(())
    }

    fn is_ready(&self) -> bool {
        !self.closed && self.child.is_some()
    }
}

/// How the demux loop holds the stdin writer.
///
/// See the construction site in [`SubprocessCliTransport::connect`] for why
/// the choice between strong and weak matters for the MCP initialize
/// handshake.
enum DemuxSender {
    /// Keeps the writer channel alive for the lifetime of the stdout reader.
    Strong(mpsc::Sender<String>),
    /// Goes dead as soon as the transport's strong `write_tx` is dropped.
    Weak(mpsc::WeakSender<String>),
}

impl DemuxSender {
    /// Obtain a usable sender if the writer channel is still open.
    fn upgrade(&self) -> Option<mpsc::Sender<String>> {
        match self {
            Self::Strong(s) => Some(s.clone()),
            Self::Weak(w) => w.upgrade(),
        }
    }
}

async fn write_stdin_loop(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
    let _ = stdin.shutdown().await;
}

async fn read_stdout_loop(
    stdout: ChildStdout,
    tx: mpsc::Sender<Result<serde_json::Value>>,
    max_buffer_size: usize,
    control_handler: Option<Arc<dyn ControlHandler>>,
    write_tx: DemuxSender,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf = String::new();
    let mut accum = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Err(ClaudeSDKError::Io(e))).await;
                break;
            }
        }
        // The CLI sometimes emits multiple JSON objects on a single line, or
        // splits one JSON across several lines. Concatenate and split on a
        // best-effort basis: try `serde_json::Deserializer::from_str` to peel
        // off complete values from the front of the accumulator.
        accum.push_str(&buf);
        if !drain_accum(
            &mut accum,
            &tx,
            control_handler.as_ref(),
            &write_tx,
            max_buffer_size,
            false,
        )
        .await
        {
            return;
        }
    }
    // Stdout reached EOF. Try one more pass on whatever is left in `accum` so
    // a final frame missing a trailing newline isn't silently dropped.
    let _ = drain_accum(
        &mut accum,
        &tx,
        control_handler.as_ref(),
        &write_tx,
        max_buffer_size,
        true,
    )
    .await;
}

/// Peel as many complete JSON values as possible off the front of `accum`.
///
/// `final_pass` flips the behavior for trailing bytes: on a normal pass an
/// incomplete tail is left in `accum` waiting for more bytes; on the final
/// pass (stdout EOF) an incomplete tail is reported as a JSON decode error so
/// the caller sees *something*.
///
/// Returns `false` when the public message channel is closed -- the caller
/// should stop reading.
async fn drain_accum(
    accum: &mut String,
    tx: &mpsc::Sender<Result<serde_json::Value>>,
    control_handler: Option<&Arc<dyn ControlHandler>>,
    write_tx: &DemuxSender,
    max_buffer_size: usize,
    final_pass: bool,
) -> bool {
    loop {
        let trimmed = accum.trim_start();
        if trimmed.is_empty() {
            accum.clear();
            return true;
        }
        let leading_ws = accum.len() - trimmed.len();
        let mut de = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();
        match de.next() {
            Some(Ok(value)) => {
                let consumed = de.byte_offset();
                accum.drain(..leading_ws + consumed);
                if !dispatch_frame(value, tx, control_handler, write_tx).await {
                    return false;
                }
            }
            Some(Err(e)) if e.is_eof() => {
                if final_pass || accum.len() > max_buffer_size {
                    let snippet = accum.clone();
                    accum.clear();
                    let _ = tx.send(Err(ClaudeSDKError::json_decode(&snippet, e))).await;
                }
                return true;
            }
            Some(Err(e)) => {
                // Drop one line -- likely a [SandboxDebug] or other
                // non-JSON line -- and try again.
                let snippet_for_err = accum.clone();
                if let Some(nl) = accum.find('\n') {
                    accum.drain(..=nl);
                } else {
                    accum.clear();
                }
                if tx
                    .send(Err(ClaudeSDKError::json_decode(&snippet_for_err, e)))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            None => {
                accum.clear();
                return true;
            }
        }
    }
}

/// Route a single decoded stdout frame.
///
/// Returns `false` if the public message channel is closed (caller dropped
/// the stream) so the read loop can exit.
async fn dispatch_frame(
    value: serde_json::Value,
    msg_tx: &mpsc::Sender<Result<serde_json::Value>>,
    control_handler: Option<&Arc<dyn ControlHandler>>,
    write_tx: &DemuxSender,
) -> bool {
    let is_control = value.get("type").and_then(|v| v.as_str()) == Some("control_request");
    if !is_control {
        return msg_tx.send(Ok(value)).await.is_ok();
    }
    let request_id = value
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let request = value
        .get("request")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let response = if let Some(handler) = control_handler {
        let req = ControlRequest {
            request_id: request_id.clone(),
            request,
        };
        handler.handle(req).await
    } else {
        ControlResponse::Error("no ControlHandler registered".to_string())
    };
    let frame = response.into_frame(&request_id);
    let mut line = frame.to_string();
    line.push('\n');
    // With a weak sender, if the caller already dropped their `write_tx`
    // (`end_input` / `close`) the upgrade returns `None`; in that case the CLI
    // is no longer expecting a response anyway, so silently skip. With a
    // strong sender (control handler present) the upgrade always succeeds, so
    // control_responses survive `end_input`.
    if let Some(sender) = write_tx.upgrade() {
        let _ = sender.send(line).await;
    }
    true
}

async fn read_stderr_loop(stderr: tokio::process::ChildStderr, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        if tx.send(line).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;
    use crate::types::PermissionMode;

    #[test]
    fn build_args_streaming_minimal() {
        let t = SubprocessCliTransport::streaming(ClaudeAgentOptions::new());
        let args = t.build_args();
        assert!(args.starts_with(&[
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ]));
        assert!(args.contains(&"--input-format".to_string()));
    }

    #[test]
    fn build_args_one_shot_uses_print() {
        let t = SubprocessCliTransport::one_shot(ClaudeAgentOptions::new(), "hi".into());
        let args = t.build_args();
        assert!(args.windows(2).any(|w| w[0] == "--print" && w[1] == "hi"));
        assert!(!args.contains(&"--input-format".to_string()));
    }

    #[test]
    fn build_args_with_model_and_permission_mode() {
        let mut opts = ClaudeAgentOptions::new();
        opts.model = Some("claude-sonnet-4-6".into());
        opts.permission_mode = Some(PermissionMode::BypassPermissions);
        opts.allowed_tools = vec!["Read".into(), "Bash".into()];
        opts.continue_conversation = true;
        let t = SubprocessCliTransport::one_shot(opts, "hi".into());
        let args = t.build_args();
        assert!(args.contains(&"--model".into()));
        assert!(args.contains(&"claude-sonnet-4-6".to_string()));
        assert!(args.contains(&"--permission-mode".into()));
        assert!(args.contains(&"bypassPermissions".to_string()));
        assert!(args.contains(&"--allowedTools".into()));
        assert!(args.contains(&"Read,Bash".to_string()));
        assert!(args.contains(&"--continue".into()));
    }

    #[test]
    fn build_args_mcp_config() {
        let mut opts = ClaudeAgentOptions::new();
        opts.mcp_servers.insert(
            "fs".into(),
            crate::types::McpServerConfig::stdio("mcp-fs", &["--root".into(), "/tmp".into()]),
        );
        opts.strict_mcp_config = true;
        let t = SubprocessCliTransport::one_shot(opts, "hi".into());
        let args = t.build_args();
        let idx = args.iter().position(|a| a == "--mcp-config").expect("flag");
        let json: serde_json::Value =
            serde_json::from_str(&args[idx + 1]).expect("valid mcp-config json");
        assert!(json["mcpServers"]["fs"]["type"] == "stdio");
        assert!(args.contains(&"--strict-mcp-config".into()));
    }

    #[test]
    fn build_args_extra_args_passthrough() {
        let mut opts = ClaudeAgentOptions::new();
        opts.extra_args
            .insert("--brand-new".into(), Some("v".into()));
        opts.extra_args.insert("--bare-flag".into(), None);
        let t = SubprocessCliTransport::streaming(opts);
        let args = t.build_args();
        assert!(args.contains(&"--brand-new".to_string()));
        assert!(args.contains(&"v".to_string()));
        assert!(args.contains(&"--bare-flag".to_string()));
    }

    #[test]
    fn build_args_includes_sdk_mcp_entry() {
        let mut opts = ClaudeAgentOptions::new();
        opts.sdk_mcp_server = Some(crate::tool::AgentToolbox::new("seher-tools"));
        let t = SubprocessCliTransport::streaming(opts);
        let args = t.build_args();
        let idx = args.iter().position(|a| a == "--mcp-config").expect("flag");
        let json: serde_json::Value =
            serde_json::from_str(&args[idx + 1]).expect("valid mcp-config json");
        assert_eq!(json["mcpServers"]["seher-tools"]["type"], "sdk");
        assert_eq!(json["mcpServers"]["seher-tools"]["name"], "seher-tools");
    }

    #[test]
    fn with_prompt_auto_registers_control_handler_for_toolbox() {
        let mut opts = ClaudeAgentOptions::new();
        opts.sdk_mcp_server = Some(crate::tool::AgentToolbox::new("seher-tools"));
        let t = SubprocessCliTransport::streaming(opts);
        assert!(t.control_handler.is_some());
    }

    #[tokio::test]
    async fn dispatch_frame_routes_message_to_msg_tx() {
        let (msg_tx, mut msg_rx) = mpsc::channel(4);
        let (write_tx, mut write_rx) = mpsc::channel::<String>(4);
        let weak = DemuxSender::Weak(write_tx.downgrade());
        let value = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "hi"}]}
        });
        assert!(dispatch_frame(value.clone(), &msg_tx, None, &weak).await);
        let got = msg_rx.recv().await.expect("got msg");
        assert_eq!(got.expect("ok"), value);
        // No control responses written.
        assert!(write_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_frame_invokes_handler_and_writes_response() {
        use crate::tool::{AgentTool, AgentToolbox, ToolboxControlHandler};
        let tb = AgentToolbox::new("svr").with_tools(vec![AgentTool::new(
            "echo",
            "echo",
            serde_json::json!({"type": "object"}),
            std::sync::Arc::new(|_| Ok("done".into())),
        )]);
        let handler: Arc<dyn ControlHandler> = Arc::new(ToolboxControlHandler::new(tb));
        let (msg_tx, mut msg_rx) = mpsc::channel(4);
        let (write_tx, mut write_rx) = mpsc::channel::<String>(4);
        let weak = DemuxSender::Weak(write_tx.downgrade());
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "mcp_message",
                "server_name": "svr",
                "message": {
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {"name": "echo", "arguments": {}}
                }
            }
        });
        assert!(dispatch_frame(frame, &msg_tx, Some(&handler), &weak).await);
        // Public message channel sees nothing.
        assert!(msg_rx.try_recv().is_err());
        // A control_response was queued.
        let line = write_rx.recv().await.expect("response line");
        let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(resp["type"], "control_response");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "req-1");
        assert_eq!(
            resp["response"]["response"]["mcp_response"]["result"]["content"][0]["text"],
            "done"
        );
    }

    #[tokio::test]
    async fn dispatch_frame_errors_without_handler() {
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (write_tx, mut write_rx) = mpsc::channel::<String>(4);
        let weak = DemuxSender::Weak(write_tx.downgrade());
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": "req-x",
            "request": {"subtype": "mcp_message"}
        });
        assert!(dispatch_frame(frame, &msg_tx, None, &weak).await);
        let line = write_rx.recv().await.expect("response line");
        let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(resp["response"]["subtype"], "error");
    }

    #[tokio::test]
    async fn dispatch_frame_skips_response_when_writer_dropped() {
        // Simulates `end_input` happening before a control_request arrives:
        // the strong sender is gone, the weak handle no longer upgrades, and
        // dispatch_frame should silently swallow the response instead of
        // panicking or hanging.
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (write_tx, _write_rx) = mpsc::channel::<String>(4);
        let weak = DemuxSender::Weak(write_tx.downgrade());
        drop(write_tx);
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": "req-z",
            "request": {"subtype": "mcp_message"}
        });
        assert!(dispatch_frame(frame, &msg_tx, None, &weak).await);
        // Nothing to verify on write_rx -- receiver was dropped too. The
        // success criterion is that the call returned without blocking.
    }

    #[tokio::test]
    async fn dispatch_frame_strong_sender_survives_end_input() {
        // Regression for #221: when an sdk_mcp_server toolbox is registered we
        // hand the demux a *strong* sender. The CLI's `mcp_message:
        // initialize` handshake arrives after the caller's `end_input()` has
        // already dropped the transport's strong `write_tx`; the demux must
        // still be able to answer it. Drop the transport-side strong sender
        // and confirm the control_response is still written.
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (write_tx, mut write_rx) = mpsc::channel::<String>(4);
        let strong = DemuxSender::Strong(write_tx.clone());
        // Simulate `end_input()` dropping the transport's only other strong
        // sender. The demux's strong clone keeps the channel open.
        drop(write_tx);
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": "req-init",
            "request": {"subtype": "mcp_message"}
        });
        assert!(dispatch_frame(frame, &msg_tx, None, &strong).await);
        let line = write_rx
            .recv()
            .await
            .expect("response line survives end_input");
        let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(resp["type"], "control_response");
        assert_eq!(resp["response"]["request_id"], "req-init");
    }
}
