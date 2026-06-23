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

/// Returns true when `msg` contains `HTTP {status}` followed by a non-digit
/// (or end of string), avoiding false positives like `HTTP 5002`.
fn contains_http_status(msg: &str, status: u16) -> bool {
    let needle = format!("HTTP {status}");
    msg.match_indices(&needle).any(|(idx, _)| {
        msg[idx + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit())
    })
}

/// Detect transient HTTP errors that are always worth retrying.
///
/// Matches full status-code substrings (`HTTP 429`, `HTTP 500`, `HTTP 502`,
/// `HTTP 503`, `HTTP 504`) to avoid false positives such as byte counts
/// containing `50` or `5029`.
#[must_use]
pub fn is_transient_http_error(msg: &str) -> bool {
    contains_http_status(msg, 429)
        || contains_http_status(msg, 500)
        || contains_http_status(msg, 502)
        || contains_http_status(msg, 503)
        || contains_http_status(msg, 504)
}

/// Detect client HTTP errors that should only be retried when explicitly opted in.
///
/// Some providers (e.g. Kimi) return 401/404 during transient outages, but
/// normally these indicate authentication or routing failures. Only retry them
/// when `retry_client_errors` is enabled.
#[must_use]
pub fn is_client_error_retryable(msg: &str) -> bool {
    contains_http_status(msg, 401) || contains_http_status(msg, 404)
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

    // -- is_transient_http_error ------------------------------------------------

    #[test]
    fn transient_detects_429_and_5xx() {
        assert!(is_transient_http_error(
            "Anthropic API error (HTTP 429): rate limited"
        ));
        assert!(is_transient_http_error(
            "Anthropic API error (HTTP 500): internal"
        ));
        assert!(is_transient_http_error(
            "Anthropic API error (HTTP 502): bad gateway"
        ));
        assert!(is_transient_http_error(
            "Anthropic API error (HTTP 503): unavailable"
        ));
        assert!(is_transient_http_error(
            "Anthropic API error (HTTP 504): timeout"
        ));
    }

    #[test]
    fn transient_rejects_4xx_and_misleading_substrings() {
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 401): auth_error"
        ));
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 403): forbidden"
        ));
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 404): not found"
        ));
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 400): bad request"
        ));
        assert!(!is_transient_http_error("connection refused"));
        assert!(!is_transient_http_error("Read 50029 bytes"));
        assert!(!is_transient_http_error("Read 5029 bytes"));
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 5002): unknown"
        ));
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 5029): unknown"
        ));
        assert!(!is_transient_http_error(
            "Anthropic API error (HTTP 4290): unknown"
        ));
    }

    // -- is_client_error_retryable ----------------------------------------------

    #[test]
    fn client_retryable_detects_401_and_404() {
        assert!(is_client_error_retryable(
            "Anthropic API error (HTTP 401): auth_error"
        ));
        assert!(is_client_error_retryable(
            "Anthropic API error (HTTP 404): not found"
        ));
    }

    #[test]
    fn client_retryable_rejects_other_statuses() {
        assert!(!is_client_error_retryable(
            "Anthropic API error (HTTP 403): forbidden"
        ));
        assert!(!is_client_error_retryable(
            "Anthropic API error (HTTP 500): internal"
        ));
        assert!(!is_client_error_retryable("connection refused"));
        assert!(!is_client_error_retryable(
            "Anthropic API error (HTTP 4012): auth_error"
        ));
        assert!(!is_client_error_retryable(
            "Anthropic API error (HTTP 4040): not found"
        ));
    }
}
