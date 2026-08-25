use chrono::{DateTime, Utc};

/// Exact reason marker for provider/network failures that may trigger provider fallback.
pub(crate) const NETWORK_ERROR_REASON: &str = "network_error";

/// Returns true when the error message is the structured network-failure marker.
#[must_use]
pub(crate) fn is_network_error_message(message: &str) -> bool {
    message == NETWORK_ERROR_REASON
}

/// Marker prefix for RPC backend failures that must never be retried.
/// Wire/state contract: matched by [`is_non_retryable_error`] callers.
pub const NON_RETRYABLE_PREFIX: &str = "Pi RPC non-retryable: ";

/// Returns true when the error message carries the non-retryable marker.
#[must_use]
pub fn is_non_retryable_error(message: &str) -> bool {
    message.starts_with(NON_RETRYABLE_PREFIX)
}

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

    /// Return true when this failure carries the exact structured
    /// network-failure marker.
    #[must_use]
    pub fn is_network_error(&self) -> bool {
        matches!(
            self,
            Self::Other { message, .. } if is_network_error_message(message)
        )
    }
    /// Return true when this failure is an HTTP 5xx server error, which is
    /// eligible for provider fallback. Messages carrying the
    /// [`NON_RETRYABLE_PREFIX`] marker are never eligible, regardless of any
    /// HTTP status their text may contain.
    #[must_use]
    pub fn is_server_error(&self) -> bool {
        matches!(self, Self::Other { message, .. } if is_server_error_message(message))
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

/// Returns true when `msg` contains `HTTP {status}` or Pi RPC's
/// `command error: {status}:` form, followed by a non-digit boundary.
pub(crate) fn contains_http_status(msg: &str, status: u16) -> bool {
    let needle = format!("HTTP {status}");
    let http = msg.match_indices(&needle).any(|(idx, _)| {
        msg[idx + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit())
    });
    http || msg.contains(&format!("command error: {status}:"))
}

fn contains_pi_command_server_error(msg: &str) -> bool {
    const PREFIX: &str = "command error: ";
    msg.match_indices(PREFIX).any(|(idx, _)| {
        let status = &msg.as_bytes()[idx + PREFIX.len()..];
        status.first() == Some(&b'5')
            && status.get(1).is_some_and(u8::is_ascii_digit)
            && status.get(2).is_some_and(u8::is_ascii_digit)
            && status.get(3) == Some(&b':')
    })
}

/// Detect HTTP/Pi RPC server errors (any 5xx status) that are eligible for
/// provider fallback after same-provider retries are exhausted.
///
/// Matches `HTTP 5` followed by exactly two more digits and a non-digit
/// boundary, or Pi's `command error: 5xx:` form. Messages carrying the
/// [`NON_RETRYABLE_PREFIX`] marker are never matched, regardless of status.
#[must_use]
pub fn is_server_error_message(msg: &str) -> bool {
    !is_non_retryable_error(msg)
        && (msg.match_indices("HTTP 5").any(|(idx, _)| {
            let rest = &msg[idx + "HTTP 5".len()..];
            let mut chars = rest.chars();
            chars.next().is_some_and(|c| c.is_ascii_digit())
                && chars.next().is_some_and(|c| c.is_ascii_digit())
                && chars.next().is_none_or(|c| !c.is_ascii_digit())
        }) || contains_pi_command_server_error(msg))
}

/// Detect transient HTTP/Pi RPC errors that are always worth retrying.
///
/// Matches HTTP 429, Pi's `command error: 429:` form, and any full HTTP/Pi
/// 5xx status-code substring, avoiding false positives such as byte counts.
#[must_use]
pub fn is_transient_http_error(msg: &str) -> bool {
    contains_http_status(msg, 429) || is_server_error_message(msg)
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
    fn transient_detects_pi_command_errors() {
        assert!(is_transient_http_error(
            "command error: 429: {\"type\":\"rate_limit_error\"}"
        ));
        assert!(is_transient_http_error(
            "command error: 503: {\"type\":\"server_error\"}"
        ));
        assert!(!is_transient_http_error("command error: 5030: unknown"));
    }

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
    // -- is_server_error_message -------------------------------------------------

    #[test]
    fn server_error_detects_5xx_and_rejects_longer_numbers() {
        assert!(is_server_error_message(
            "Anthropic API error (HTTP 500): internal"
        ));
        assert!(is_server_error_message(
            "Anthropic API error (HTTP 503): unavailable"
        ));
        assert!(is_server_error_message(
            "Anthropic API error (HTTP 529): overloaded"
        ));
        assert!(is_server_error_message(
            "command error: 503: {\"type\":\"server_error\"}"
        ));
        assert!(!is_server_error_message(
            "Anthropic API error (HTTP 499): client"
        ));
        assert!(!is_server_error_message(
            "Anthropic API error (HTTP 5002): unknown"
        ));
        assert!(!is_server_error_message("command error: 5030: unknown"));
        assert!(!is_server_error_message("command error: 400: bad request"));
        assert!(!is_server_error_message("command error: 503"));
        assert!(!is_server_error_message("command error: 50: incomplete"));
        assert!(!is_server_error_message("command error: 5: incomplete"));
        // Truncated status at a boundary must be rejected.
        assert!(!is_server_error_message("request failed (HTTP 5)"));
        assert!(!is_server_error_message("request failed HTTP 5"));
        assert!(!is_server_error_message("Read 5029 bytes"));
        assert!(!is_server_error_message("network_error"));
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
    #[test]
    fn server_error_excludes_non_retryable_marker() {
        let marked_message =
            format!("{NON_RETRYABLE_PREFIX}Pi RPC process exited while prompting: HTTP 500");
        assert!(!is_server_error_message(&marked_message));
        let marked = RunError::Other {
            message: marked_message,
            partial: String::new(),
        };
        assert!(!marked.is_server_error());
        assert!(!is_server_error_message(&format!(
            "{NON_RETRYABLE_PREFIX}child exited: HTTP 503"
        )));
        let plain = RunError::Other {
            message: "Anthropic API error (HTTP 500): internal".to_string(),
            partial: String::new(),
        };
        assert!(plain.is_server_error());
    }

    #[test]
    fn run_error_classification_is_false_for_limit_and_timeout() {
        // Guards against a later broadening of the matches! over all RunError
        // variants misclassifying Limit/Timeout as fallback-eligible.
        let limit = RunError::Limit {
            error: LimitError {
                provider: "claude".to_string(),
                reset_at: None,
            },
            partial: String::new(),
        };
        let timeout = RunError::Timeout {
            error: TimeoutError {
                ms: 1000,
                label: "test",
            },
            partial: String::new(),
        };
        assert!(!limit.is_server_error());
        assert!(!timeout.is_server_error());
        assert!(!limit.is_network_error());
        assert!(!timeout.is_network_error());
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
        assert!(is_client_error_retryable("command error: 401: auth_error"));
        assert!(is_client_error_retryable("command error: 404: not found"));
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
    #[test]
    fn network_marker_requires_exact_reason() {
        assert!(is_network_error_message(NETWORK_ERROR_REASON));
        assert!(!is_network_error_message("network_error: connection reset"));
        assert!(!is_network_error_message("Network_Error"));
    }

    #[test]
    fn network_run_error_exposes_partial_and_type() {
        let error = RunError::Other {
            message: NETWORK_ERROR_REASON.to_string(),
            partial: "before failure".to_string(),
        };
        assert!(error.is_network_error());
        assert_eq!(error.partial(), "before failure");
    }
}
