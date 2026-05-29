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
