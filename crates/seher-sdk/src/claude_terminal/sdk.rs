use std::time::{SystemTime, UNIX_EPOCH};

use super::command::{BuildClaudeCommandOptions, build_claude_command};
use super::detect::{build_needles, detect_session_limit, paste_is_consumed};
use super::normalizer::normalize_text;
use super::transcript::{FileSystemTranscriptReader, default_transcript_root, encode_project_dir};
use super::types::{
    ClaudeRunOutput, ClaudeSessionRef, ClaudeTerminalError, ClaudeTerminalResponse,
    ClaudeTranscriptReader, FindClaudeSessionOptions, TerminalBackend, TerminalSession,
    TerminalStartOptions, WaitForAssistantResponseOptions,
};
use crate::sdk::util::encode_session_id;

const DEFAULT_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 500;
const DEFAULT_READY_TIMEOUT_MS: u64 = 30 * 1000;
const DEFAULT_PASTE_VISIBLE_TIMEOUT_MS: u64 = 90 * 1000;
const DEFAULT_READY_POLL_INTERVAL_MS: u64 = 100;
// sakoku-ignore-next-line
const DEFAULT_READY_INDICATOR: &str = "❯";
const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";
const CAPTURE_FAILURE_LIMIT: usize = 3;

#[derive(Default)]
pub struct ClaudeTerminalSdkConfig {
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    pub ready_timeout_ms: Option<u64>,
    pub paste_visible_timeout_ms: Option<u64>,
    pub poll_interval_ms: Option<u64>,
    pub ready_poll_interval_ms: Option<u64>,
    pub claude_bin: Option<String>,
    pub transcript_root: Option<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub keep_session: bool,
    pub ready_indicator: Option<String>,
    /// Required. Use [`new_sdk_with_defaults`] to get a pre-wired instance.
    pub backend: Option<Box<dyn TerminalBackend>>,
    /// Required. Use [`new_sdk_with_defaults`] to get a pre-wired instance.
    pub reader: Option<Box<dyn ClaudeTranscriptReader>>,
}

pub struct ClaudeTerminalSdk {
    config: ClaudeTerminalSdkConfig,
}

impl ClaudeTerminalSdk {
    #[must_use]
    pub fn new(config: ClaudeTerminalSdkConfig) -> Self {
        Self { config }
    }

    /// Execute a prompt and return the full text response plus the session id.
    ///
    /// `resume` continues a prior Claude session (`claude --resume <id>`); `None` starts
    /// a fresh session whose newly generated id is reported in [`ClaudeRunOutput`].
    ///
    /// # Errors
    ///
    /// Returns `ClaudeTerminalError` on tmux/spawn failures, timeouts, or session-limit.
    pub fn run(
        &self,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<ClaudeRunOutput, ClaudeTerminalError> {
        let response = self.execute(prompt, resume)?;
        Ok(ClaudeRunOutput {
            text: normalize_text(&response),
            session_id: response.session_id,
        })
    }

    fn now() -> u64 {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        u64::try_from(ms).unwrap_or(u64::MAX)
    }

    fn execute(
        &self,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<ClaudeTerminalResponse, ClaudeTerminalError> {
        let cwd = self.config.cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map_or_else(|_| ".".to_string(), |p| p.to_string_lossy().into_owned())
        });
        let timeout_ms = self.config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let poll_ms = self
            .config
            .poll_interval_ms
            .unwrap_or(DEFAULT_POLL_INTERVAL_MS);
        let ready_timeout_ms = self
            .config
            .ready_timeout_ms
            .unwrap_or(DEFAULT_READY_TIMEOUT_MS);
        let paste_visible_ms = self
            .config
            .paste_visible_timeout_ms
            .unwrap_or(DEFAULT_PASTE_VISIBLE_TIMEOUT_MS);
        let ready_poll_ms = self
            .config
            .ready_poll_interval_ms
            .unwrap_or(DEFAULT_READY_POLL_INTERVAL_MS);
        let ready_indicator = self
            .config
            .ready_indicator
            .as_deref()
            .unwrap_or(DEFAULT_READY_INDICATOR);
        let transcript_root = self
            .config
            .transcript_root
            .clone()
            .unwrap_or_else(default_transcript_root);

        let command = build_claude_command(&BuildClaudeCommandOptions {
            claude_bin: self
                .config
                .claude_bin
                .clone()
                .unwrap_or_else(|| "claude".to_string()),
            permission_mode: self
                .config
                .permission_mode
                .clone()
                .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_string()),
            model: self.config.model.clone(),
            // On resume the system prompt is already part of the persisted session;
            // re-passing `--append-system-prompt` would duplicate it. Apply it only when
            // starting a fresh session.
            system_prompt: if resume.is_some() {
                None
            } else {
                self.config.system_prompt.clone()
            },
            resume_session_id: resume.map(str::to_string),
        })?;

        // Only needed for the fresh-session path, which detects the new transcript by
        // diffing against the names that already existed at launch.
        let exclude_names = self.reader().list_session_names(&transcript_root, &cwd)?;
        let started_at_ms = Self::now();

        let session = self.backend().start(TerminalStartOptions {
            cwd: cwd.clone(),
            command,
            env: None,
        })?;

        let result = self.run_session(
            &session,
            prompt,
            &cwd,
            &transcript_root,
            &exclude_names,
            resume,
            started_at_ms,
            timeout_ms,
            poll_ms,
            ready_timeout_ms,
            paste_visible_ms,
            ready_poll_ms,
            ready_indicator,
        );

        if !self.config.keep_session {
            let _ = self.backend().stop(&session);
        }

        result
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "timeline parameters mirror the TS design"
    )]
    fn run_session(
        &self,
        session: &TerminalSession,
        prompt: &str,
        cwd: &str,
        transcript_root: &str,
        exclude_names: &std::collections::HashSet<String>,
        resume: Option<&str>,
        started_at_ms: u64,
        timeout_ms: u64,
        poll_ms: u64,
        ready_timeout_ms: u64,
        paste_visible_ms: u64,
        ready_poll_ms: u64,
        ready_indicator: &str,
    ) -> Result<ClaudeTerminalResponse, ClaudeTerminalError> {
        self.wait_for_ready(session, ready_indicator, ready_timeout_ms, ready_poll_ms)?;
        self.backend().paste_text(session, prompt)?;
        self.wait_for_paste_visible(session, prompt, paste_visible_ms, ready_poll_ms)?;
        self.backend().submit(session)?;

        // Resume appends to the existing `<id>.jsonl`, so there is no new file to find --
        // the transcript path is derived directly from the id + cwd. A fresh session
        // instead waits for a brand-new transcript to appear.
        let session_ref = match resume {
            Some(id) => ClaudeSessionRef {
                session_id: id.to_string(),
                transcript_path: encode_transcript_path(transcript_root, cwd, id),
            },
            None => self.reader().find_session(FindClaudeSessionOptions {
                cwd: cwd.to_string(),
                after_ms: started_at_ms,
                timeout_ms,
                poll_interval_ms: poll_ms,
                root: transcript_root.to_string(),
                exclude_names: exclude_names.clone(),
            })?,
        };

        self.reader().wait_for_assistant_response(
            &session_ref,
            WaitForAssistantResponseOptions {
                timeout_ms,
                poll_interval_ms: poll_ms,
                // Exclude prior turns already present in a resumed transcript.
                after_ms: started_at_ms,
            },
        )
    }

    /// Capture the screen, retrying up to `CAPTURE_FAILURE_LIMIT` times on error.
    /// Returns `Ok(None)` when the capture failed (error consumed into counter) and
    /// `Err` when the failure limit is reached.
    fn capture_with_retry(
        &self,
        session: &TerminalSession,
        consecutive_failures: &mut usize,
        context: &str,
    ) -> Result<Option<String>, ClaudeTerminalError> {
        match self.backend().capture_screen(session) {
            Ok(s) => {
                *consecutive_failures = 0;
                Ok(Some(s))
            }
            Err(e) => {
                *consecutive_failures += 1;
                if *consecutive_failures >= CAPTURE_FAILURE_LIMIT {
                    return Err(ClaudeTerminalError::Other(format!(
                        "captureScreen failed {consecutive_failures} times in a row while {context}: {e}"
                    )));
                }
                Ok(None)
            }
        }
    }

    fn wait_for_ready(
        &self,
        session: &TerminalSession,
        indicator: &str,
        timeout_ms: u64,
        poll_ms: u64,
    ) -> Result<(), ClaudeTerminalError> {
        let deadline = Self::now().saturating_add(timeout_ms);
        let mut consecutive_failures = 0usize;
        loop {
            let screen = self
                .capture_with_retry(
                    session,
                    &mut consecutive_failures,
                    "waiting for Claude TUI to render",
                )?
                .unwrap_or_default();

            // Check for session limit BEFORE checking the ready indicator
            if let Some(reset_info) = detect_session_limit(&screen) {
                return Err(ClaudeTerminalError::SessionLimit { reset_info });
            }
            if screen.contains(indicator) {
                return Ok(());
            }
            if Self::now() >= deadline {
                return Err(ClaudeTerminalError::Timeout(format!(
                    "timed out waiting for Claude TUI to render (no \"{indicator}\" within {timeout_ms}ms)"
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
        }
    }

    fn wait_for_paste_visible(
        &self,
        session: &TerminalSession,
        prompt: &str,
        timeout_ms: u64,
        poll_ms: u64,
    ) -> Result<(), ClaudeTerminalError> {
        let needles = build_needles(prompt);
        // Empty-needle short-circuit (whitespace-only / empty prompt)
        if paste_is_consumed("", &needles) {
            return Ok(());
        }
        let deadline = Self::now().saturating_add(timeout_ms);
        let mut consecutive_failures = 0usize;
        loop {
            let screen = self
                .capture_with_retry(
                    session,
                    &mut consecutive_failures,
                    "waiting for pasted prompt to render",
                )?
                .unwrap_or_default();

            if paste_is_consumed(&screen, &needles) {
                return Ok(());
            }
            if Self::now() >= deadline {
                return Err(ClaudeTerminalError::Timeout(format!(
                    "timed out waiting for pasted prompt to appear in Claude TUI within {timeout_ms}ms \
                     (prefix {prefix_len} chars, suffix {suffix_len} chars)",
                    prefix_len = needles.prefix.len(),
                    suffix_len = needles.suffix.len(),
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
        }
    }

    fn backend(&self) -> &dyn TerminalBackend {
        self.config.backend.as_deref().unwrap_or_else(|| {
            panic!("ClaudeTerminalSdk: no backend set -- use new_sdk_with_defaults()")
        })
    }

    fn reader(&self) -> &dyn ClaudeTranscriptReader {
        self.config.reader.as_deref().unwrap_or_else(|| {
            panic!("ClaudeTerminalSdk: no reader set -- use new_sdk_with_defaults()")
        })
    }
}

/// Build the transcript file path Claude Code uses for `session_id` under `cwd`:
/// `<root>/<encoded-cwd>/<session_id>.jsonl`.
#[must_use]
pub fn encode_transcript_path(root: &str, cwd: &str, session_id: &str) -> String {
    std::path::Path::new(root)
        .join(encode_project_dir(cwd))
        .join(format!("{}.jsonl", encode_session_id(session_id)))
        .to_string_lossy()
        .into_owned()
}

/// Convenience builder that wires up real `TmuxBackend` + `FileSystemTranscriptReader`.
#[must_use]
pub fn new_sdk_with_defaults(
    claude_bin: Option<String>,
    tmux_bin: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    timeout_ms: Option<u64>,
    cwd: Option<String>,
) -> ClaudeTerminalSdk {
    use super::tmux_backend::TmuxBackend;
    ClaudeTerminalSdk::new(ClaudeTerminalSdkConfig {
        claude_bin,
        model,
        system_prompt,
        timeout_ms,
        cwd,
        backend: Some(Box::new(TmuxBackend::new(tmux_bin))),
        reader: Some(Box::new(FileSystemTranscriptReader::new())),
        ..Default::default()
    })
}

/// Run a prompt through `ClaudeTerminalSdk` on a dedicated thread,
/// emitting `StreamChunk`s compatible with seher-cli's `drain_to_stdout`.
///
/// `resume` continues a prior Claude session id; `None` starts a fresh one. The
/// resulting session id is emitted via [`StreamChunk::Session`] before the text chunk --
/// though, since Claude assigns the id mid-run, nothing is emitted until the run
/// completes (the whole response arrives as a single `Delta`).
#[must_use]
pub fn stream_via_thread(
    sdk: ClaudeTerminalSdk,
    prompt: String,
    provider_label: String,
    resume: Option<String>,
) -> std::sync::mpsc::Receiver<crate::sdk::StreamChunk> {
    use crate::sdk::{LimitError, StreamChunk};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || match sdk.run(&prompt, resume.as_deref()) {
        Ok(output) => {
            let _ = tx.send(StreamChunk::Session(output.session_id));
            let _ = tx.send(StreamChunk::Delta(output.text));
            let _ = tx.send(StreamChunk::Done(String::new()));
        }
        Err(ClaudeTerminalError::SessionLimit { reset_info: _ }) => {
            let _ = tx.send(StreamChunk::Limit(LimitError {
                provider: provider_label,
                reset_at: None,
            }));
        }
        Err(e) => {
            let _ = tx.send(StreamChunk::Error(e.to_string()));
        }
    });
    rx
}
