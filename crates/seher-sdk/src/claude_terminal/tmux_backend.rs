use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use uuid::Uuid;

use super::types::{ClaudeTerminalError, TerminalBackend, TerminalSession, TerminalStartOptions};

const DEFAULT_TMUX_BIN: &str = "tmux";
const DEFAULT_SESSION_PREFIX: &str = "seher-claude";

struct SpawnResult {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

pub struct TmuxBackend {
    tmux_bin: String,
    session_prefix: String,
}

impl TmuxBackend {
    #[must_use]
    pub fn new(tmux_bin: Option<String>) -> Self {
        Self {
            tmux_bin: tmux_bin.unwrap_or_else(|| DEFAULT_TMUX_BIN.to_string()),
            session_prefix: DEFAULT_SESSION_PREFIX.to_string(),
        }
    }

    fn run_tmux(
        &self,
        label: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<SpawnResult, ClaudeTerminalError> {
        let res = spawn(&self.tmux_bin, args, stdin, None).map_err(|e| {
            ClaudeTerminalError::Other(format!("failed to spawn tmux {label}: {e}"))
        })?;
        if res.exit_code != Some(0) {
            return Err(ClaudeTerminalError::Other(format!(
                "tmux {label} failed (exit {:?}): {}",
                res.exit_code,
                res.stderr.trim()
            )));
        }
        Ok(res)
    }
}

impl TerminalBackend for TmuxBackend {
    fn start(
        &self,
        options: TerminalStartOptions,
    ) -> Result<TerminalSession, ClaudeTerminalError> {
        // Use full UUID to avoid session-name guessing attacks
        let id = format!("{}-{}", self.session_prefix, Uuid::new_v4());
        let mut args: Vec<&str> = vec!["new-session", "-d", "-s", &id, "-c", &options.cwd];
        for cmd in &options.command {
            args.push(cmd.as_str());
        }

        let res = spawn(&self.tmux_bin, &args, None, options.env.as_ref())
            .map_err(|e| {
                ClaudeTerminalError::Other(format!("failed to spawn tmux new-session: {e}"))
            })?;
        if res.exit_code != Some(0) {
            return Err(ClaudeTerminalError::Other(format!(
                "tmux new-session failed (exit {:?}): {}",
                res.exit_code,
                res.stderr.trim()
            )));
        }
        Ok(TerminalSession { id })
    }

    fn paste_text(
        &self,
        session: &TerminalSession,
        text: &str,
    ) -> Result<(), ClaudeTerminalError> {
        let buffer_name = format!("{}-prompt", session.id);
        // load-buffer reads text from stdin
        self.run_tmux(
            "load-buffer",
            &["load-buffer", "-b", &buffer_name, "-"],
            Some(text),
        )?;
        // paste-buffer: bracketed paste event to the TUI — no Enter sent
        let paste_result = self.run_tmux(
            "paste-buffer",
            &["paste-buffer", "-b", &buffer_name, "-t", &session.id],
            None,
        );
        // Always attempt cleanup even if paste failed
        let delete_result = self.run_tmux(
            "delete-buffer",
            &["delete-buffer", "-b", &buffer_name],
            None,
        );
        paste_result?;
        delete_result?;
        Ok(())
    }

    fn submit(&self, session: &TerminalSession) -> Result<(), ClaudeTerminalError> {
        self.run_tmux("send-keys Enter", &["send-keys", "-t", &session.id, "Enter"], None)?;
        Ok(())
    }

    fn capture_screen(&self, session: &TerminalSession) -> Result<String, ClaudeTerminalError> {
        let res = self.run_tmux(
            "capture-pane",
            &["capture-pane", "-p", "-t", &session.id],
            None,
        )?;
        Ok(res.stdout)
    }

    fn stop(&self, session: &TerminalSession) -> Result<(), ClaudeTerminalError> {
        let buffer_name = format!("{}-prompt", session.id);
        // best-effort buffer cleanup (may not exist)
        let _ = self.run_tmux(
            "delete-buffer",
            &["delete-buffer", "-b", &buffer_name],
            None,
        );
        self.run_tmux("kill-session", &["kill-session", "-t", &session.id], None)?;
        Ok(())
    }
}

fn spawn(
    bin: &str,
    args: &[&str],
    stdin: Option<&str>,
    env: Option<&HashMap<String, String>>,
) -> Result<SpawnResult, String> {
    let stdin_pipe = stdin.is_some();
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.stdin(if stdin_pipe { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if let Some(extra_env) = env {
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;
    if let (Some(text), Some(mut stdin_handle)) = (stdin, child.stdin.take()) {
        stdin_handle
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write stdin to {bin}: {e}"))?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok(SpawnResult {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
