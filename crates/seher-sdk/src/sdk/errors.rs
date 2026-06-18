use chrono::{DateTime, Utc};

#[derive(Debug, thiserror::Error)]
#[error("Provider '{provider}' hit API rate/usage limit")]
pub struct LimitError {
    pub provider: String,
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
#[error("seher '{label}' timed out after {ms}ms")]
pub struct TimeoutError {
    pub ms: u64,
    pub label: &'static str,
}

/// Errors returned by [`crate::sdk::PiRunner::run`]. Each variant carries any
/// `partial` assistant text accumulated before the failure (may be empty).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{error} (partial output: {} chars)", partial.len())]
    Limit {
        #[source]
        error: LimitError,
        partial: String,
    },
    #[error("{error} (partial output: {} chars)", partial.len())]
    Timeout {
        #[source]
        error: TimeoutError,
        partial: String,
    },
    #[error("{message} (partial output: {} chars)", partial.len())]
    Other { message: String, partial: String },
}

impl RunError {
    /// Return the partial text accumulated before the failure (may be empty).
    #[must_use]
    pub fn partial(&self) -> &str {
        match self {
            Self::Limit { partial, .. }
            | Self::Timeout { partial, .. }
            | Self::Other { partial, .. } => partial,
        }
    }
}

/// Heuristic rate-limit / usage-limit detector for free-form error messages
/// emitted by the Claude CLI family (claude-headless, claude-agent-sdk).
///
/// Both backends surface identical wording for these conditions, so a single
/// shared classifier avoids the two copies drifting when new phrases appear.
#[must_use]
pub fn is_claude_rate_limit_message(msg: &str) -> bool {
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
    fn detects_common_phrases() {
        assert!(is_claude_rate_limit_message("Error: rate limit exceeded"));
        assert!(is_claude_rate_limit_message("Too Many Requests"));
        assert!(is_claude_rate_limit_message("session limit reached"));
        assert!(is_claude_rate_limit_message("usage limit"));
        assert!(!is_claude_rate_limit_message("regular text"));
        assert!(!is_claude_rate_limit_message(""));
    }
}
