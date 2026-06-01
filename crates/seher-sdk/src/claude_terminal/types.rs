use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct TerminalStartOptions {
    pub cwd: String,
    pub command: Vec<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub id: String,
}

pub trait TerminalBackend: Send + Sync {
    /// # Errors
    /// Returns an error if the tmux session cannot be started.
    fn start(&self, options: TerminalStartOptions) -> Result<TerminalSession, ClaudeTerminalError>;
    /// Send text as paste without Enter — callers verify render then call `submit()`.
    ///
    /// # Errors
    /// Returns an error if the paste operation fails.
    fn paste_text(
        &self,
        session: &TerminalSession,
        text: &str,
    ) -> Result<(), ClaudeTerminalError>;
    /// # Errors
    /// Returns an error if the Enter keystroke cannot be sent.
    fn submit(&self, session: &TerminalSession) -> Result<(), ClaudeTerminalError>;
    /// # Errors
    /// Returns an error if the screen capture fails.
    fn capture_screen(&self, session: &TerminalSession) -> Result<String, ClaudeTerminalError>;
    /// # Errors
    /// Returns an error if the session cannot be stopped.
    fn stop(&self, session: &TerminalSession) -> Result<(), ClaudeTerminalError>;
}

#[derive(Debug, Clone)]
pub struct ClaudeSessionRef {
    pub session_id: String,
    pub transcript_path: String,
}

#[derive(Debug, Clone)]
pub struct FindClaudeSessionOptions {
    pub cwd: String,
    pub after_ms: u64,
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub root: String,
    pub exclude_names: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct WaitForAssistantResponseOptions {
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
}

pub trait ClaudeTranscriptReader: Send + Sync {
    /// # Errors
    /// Returns an error if the transcript directory cannot be read.
    fn list_session_names(
        &self,
        root: &str,
        cwd: &str,
    ) -> Result<std::collections::HashSet<String>, ClaudeTerminalError>;
    /// # Errors
    /// Returns `ClaudeTerminalError::Timeout` if no new session appears within the timeout.
    fn find_session(
        &self,
        options: FindClaudeSessionOptions,
    ) -> Result<ClaudeSessionRef, ClaudeTerminalError>;
    /// # Errors
    /// Returns `ClaudeTerminalError::Timeout` if no assistant response appears within the timeout.
    fn wait_for_assistant_response(
        &self,
        session: &ClaudeSessionRef,
        options: WaitForAssistantResponseOptions,
    ) -> Result<ClaudeTerminalResponse, ClaudeTerminalError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<MessageContent>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClaudeTerminalResponse {
    pub session_id: String,
    pub assistant_messages: Vec<TranscriptMessage>,
    pub last_result_message: Option<TranscriptMessage>,
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ClaudeTerminalError {
    #[error("{0}")]
    Other(String),
    #[error("{0}")]
    Timeout(String),
    /// Claude TUI showed the "session limit" banner. Retriable after reset.
    #[error("{}", format_session_limit(.reset_info.as_deref()))]
    SessionLimit { reset_info: Option<String> },
}

fn format_session_limit(reset_info: Option<&str>) -> String {
    match reset_info {
        Some(r) => format!("Claude session limit reached (resets {r})"),
        None => "Claude session limit reached".to_string(),
    }
}

impl ClaudeTerminalError {
    #[must_use]
    pub fn is_session_limit(&self) -> bool {
        matches!(self, Self::SessionLimit { .. })
    }
    #[must_use]
    pub fn reset_info(&self) -> Option<&str> {
        match self {
            Self::SessionLimit { reset_info } => reset_info.as_deref(),
            _ => None,
        }
    }
}
