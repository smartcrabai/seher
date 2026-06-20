//! `claude -p` subprocess runner.
//!
//! Spawns `claude -p "<prompt>"` as a child process and streams stdout back as
//! `StreamChunk`s. Much simpler than `claude-terminal` (no tmux, no transcript
//! polling) — just a blocking subprocess call.

use std::io::Read as _;
use std::process::{Command, Stdio};

use crate::sdk::{CancelToken, LimitError, StreamChunk, is_claude_rate_limit_message};

const DEFAULT_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

#[derive(Default)]
#[non_exhaustive]
pub struct ClaudeHeadlessRunnerConfig {
    pub claude_bin: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub timeout_ms: Option<u64>,
    pub cwd: Option<String>,
    pub resume_session_id: Option<String>,
    pub cancel: CancelToken,
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

        if self.config.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {bin}: {e}"))?;

        let timeout =
            std::time::Duration::from_millis(self.config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        // Read stdout and stderr on separate threads to avoid deadlock when
        // the OS pipe buffer for either stream fills up.
        let mut stdout_handle = child.stdout.take().map(|mut r| {
            std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = r.read_to_string(&mut buf);
                buf
            })
        });
        let mut stderr_handle = child.stderr.take().map(|mut r| {
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
                    if self.config.cancel.is_cancelled() {
                        let _ = child.kill();
                        let _ = child.wait();
                        // Child is dead; its pipe write-ends are now closed so
                        // the reader threads will see EOF and return quickly.
                        if let Some(h) = stdout_handle.take() {
                            let _ = h.join();
                        }
                        if let Some(h) = stderr_handle.take() {
                            let _ = h.join();
                        }
                        return Err("cancelled".to_string());
                    }
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        // Wait so the OS can reap the zombie immediately.
                        let _ = child.wait();
                        if let Some(h) = stdout_handle.take() {
                            let _ = h.join();
                        }
                        if let Some(h) = stderr_handle.take() {
                            let _ = h.join();
                        }
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
            if is_claude_rate_limit_message(&e) {
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
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

    // Rate-limit phrase detection is covered by
    // `sdk::errors::is_claude_rate_limit_message` tests.

    #[test]
    fn run_returns_err_when_cancel_token_is_already_set() {
        // Given: a cancel token that has already been cancelled before run() is called
        let cancel = CancelToken::new();
        cancel.cancel();
        let runner = ClaudeHeadlessRunner::new(ClaudeHeadlessRunnerConfig {
            // Use a non-existent bin so that if the cancel check is accidentally
            // skipped, spawn() fails quickly rather than blocking indefinitely.
            claude_bin: Some("true".to_string()),
            cancel,
            ..Default::default()
        });
        // When: run() is called with an already-cancelled token
        // Then: returns Err containing "cancel" without blocking
        match runner.run("hello") {
            Err(e) => assert!(
                e.contains("cancel"),
                "expected a cancellation error, got: {e}"
            ),
            Ok(_) => panic!("expected Err when cancel token is set"),
        }
    }

    #[test]
    fn run_kills_child_when_cancel_fires_during_wait() {
        // Given: a long-running subprocess and a cancel that fires concurrently.
        //
        // Strategy: use a temporary executable script as `claude_bin` so that
        // it is invoked as `<wrapper> -p <prompt> --permission-mode bypassPermissions`.
        // The shebang causes the OS to run `/bin/sh <wrapper> <args...>`, where the
        // remaining args become positional parameters that the script ignores.
        // This avoids relying on `/bin/sh -p` (dash rejects -p as "Illegal option").
        use std::os::unix::fs::PermissionsExt as _;

        // Use TempDir + File so the write FD is closed before exec.
        // On Linux, executing a file whose write FD is still open yields
        // ETXTBSY (Text file busy, os error 26).
        let tmp_dir = tempfile::TempDir::new().expect("create tmpdir");
        let wrapper_path = tmp_dir.path().join("wrapper.sh");
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&wrapper_path).expect("create wrapper");
            writeln!(f, "#!/bin/sh").expect("write shebang");
            writeln!(f, "sleep 60").expect("write sleep");
            // f drops here, closing the FD before exec
        }
        let mut perms = std::fs::metadata(&wrapper_path)
            .expect("get perms")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper_path, perms).expect("set perms");
        let wrapper_bin = wrapper_path.to_str().expect("path to str").to_string();

        let cancel = CancelToken::new();
        let cancel_for_thread = cancel.clone();
        let runner = ClaudeHeadlessRunner::new(ClaudeHeadlessRunnerConfig {
            claude_bin: Some(wrapper_bin),
            cancel,
            ..Default::default()
        });

        // Cancel after a short delay from a background thread.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            cancel_for_thread.cancel();
        });

        // When: run() is called — it should be interrupted by the cancel
        // Then: returns Err containing "cancel" well before the 60-second sleep ends
        let start = std::time::Instant::now();
        let result = runner.run("ignored-prompt");
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs() < 5,
            "run() should have been cancelled quickly, took {elapsed:?}"
        );
        match result {
            Err(e) => assert!(
                e.contains("cancel"),
                "expected cancellation error, got: {e}"
            ),
            Ok(_) => panic!("expected Err when cancel fires during wait"),
        }
    }
}
