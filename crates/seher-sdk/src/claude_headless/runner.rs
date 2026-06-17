//! `claude -p` subprocess runner.
//!
//! Spawns `claude -p "<prompt>"` as a child process and streams stdout back as
//! `StreamChunk`s. Much simpler than `claude-terminal` (no tmux, no transcript
//! polling) — just a blocking subprocess call.

use std::io::Read as _;
use std::process::{Command, Stdio};

use crate::sdk::{LimitError, StreamChunk};

const DEFAULT_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

#[derive(Default)]
pub struct ClaudeHeadlessRunnerConfig {
    pub claude_bin: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub timeout_ms: Option<u64>,
    pub cwd: Option<String>,
    pub resume_session_id: Option<String>,
}

pub struct ClaudeHeadlessRunner {
    config: ClaudeHeadlessRunnerConfig,
}

impl ClaudeHeadlessRunner {
    #[must_use]
    pub fn new(config: ClaudeHeadlessRunnerConfig) -> Self {
        Self { config }
    }

    /// Build the argument list for `claude -p`.
    fn build_args(&self, prompt: &str) -> Vec<String> {
        let mut args = Vec::new();

        if let Some(id) = &self.config.resume_session_id {
            args.push("--resume".to_string());
            args.push(id.clone());
        }

        args.push("-p".to_string());
        args.push(prompt.to_string());

        if let Some(m) = &self.config.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }

        if let Some(s) = &self.config.system_prompt {
            args.push("--append-system-prompt".to_string());
            args.push(s.clone());
        }

        let mode = self
            .config
            .permission_mode
            .as_deref()
            .unwrap_or(DEFAULT_PERMISSION_MODE);
        args.push("--permission-mode".to_string());
        args.push(mode.to_string());

        args
    }

    /// Run `claude -p` and return the full stdout output.
    ///
    /// # Errors
    ///
    /// Returns a string error on spawn failure, non-zero exit, or timeout.
    pub fn run(&self, prompt: &str) -> Result<String, String> {
        let bin = self.config.claude_bin.as_deref().unwrap_or("claude");
        let args = self.build_args(prompt);

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(cwd) = &self.config.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {bin}: {e}"))?;

        let timeout =
            std::time::Duration::from_millis(self.config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        // Read stdout and stderr on separate threads to avoid deadlock when
        // the OS pipe buffer for either stream fills up.
        let stdout_handle = child.stdout.take().map(|mut r| {
            std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = r.read_to_string(&mut buf);
                buf
            })
        });
        let stderr_handle = child.stderr.take().map(|mut r| {
            std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = r.read_to_string(&mut buf);
                buf
            })
        });

        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        return Err(format!(
                            "claude -p timed out after {}ms",
                            timeout.as_millis()
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => return Err(format!("error waiting for claude process: {e}")),
            }
        }

        let status = child
            .wait()
            .map_err(|e| format!("error waiting for claude process: {e}"))?;

        let output = stdout_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr_out = stderr_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();

        if !status.success() {
            let code = status
                .code()
                .map_or("signal".to_string(), |c| c.to_string());
            return Err(format!("claude -p exited with {code}: {stderr_out}"));
        }

        Ok(output)
    }
}

/// Run a prompt through `claude -p` on a dedicated thread, emitting
/// `StreamChunk`s compatible with seher-cli's `drain_to_stdout`.
#[must_use]
pub fn stream_headless(
    runner: ClaudeHeadlessRunner,
    prompt: String,
    provider_label: String,
) -> std::sync::mpsc::Receiver<StreamChunk> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || match runner.run(&prompt) {
        Ok(text) => {
            let _ = tx.send(StreamChunk::Delta(text));
            let _ = tx.send(StreamChunk::Done(String::new()));
        }
        Err(e) => {
            if is_headless_limit(&e) {
                let _ = tx.send(StreamChunk::Limit(LimitError {
                    provider: provider_label,
                    reset_at: None,
                }));
            } else {
                let _ = tx.send(StreamChunk::Error(e));
            }
        }
    });
    rx
}

fn is_headless_limit(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("usage limit")
        || lower.contains("too many requests")
        || lower.contains("session limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_minimal() {
        let runner = ClaudeHeadlessRunner::new(ClaudeHeadlessRunnerConfig::default());
        let args = runner.build_args("hello");
        assert_eq!(
            args,
            ["-p", "hello", "--permission-mode", "bypassPermissions"]
        );
    }

    #[test]
    fn build_args_with_model_and_system() {
        let runner = ClaudeHeadlessRunner::new(ClaudeHeadlessRunnerConfig {
            model: Some("claude-sonnet-4-6".to_string()),
            system_prompt: Some("Be concise.".to_string()),
            ..Default::default()
        });
        let args = runner.build_args("hello");
        assert_eq!(
            args,
            [
                "-p",
                "hello",
                "--model",
                "claude-sonnet-4-6",
                "--append-system-prompt",
                "Be concise.",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
    }

    #[test]
    fn build_args_with_resume() {
        let runner = ClaudeHeadlessRunner::new(ClaudeHeadlessRunnerConfig {
            resume_session_id: Some("abc-123".to_string()),
            ..Default::default()
        });
        let args = runner.build_args("hello");
        assert_eq!(
            args,
            [
                "--resume",
                "abc-123",
                "-p",
                "hello",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
    }

    #[test]
    fn is_limit_detection() {
        assert!(is_headless_limit("Error: rate limit exceeded"));
        assert!(is_headless_limit("Too Many Requests"));
        assert!(is_headless_limit("session limit reached"));
        assert!(!is_headless_limit("normal output text"));
    }
}
