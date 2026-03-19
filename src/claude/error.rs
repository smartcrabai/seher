use thiserror::Error;

#[derive(Error, Debug)]
pub enum ClaudeApiError {
    #[error("HTTP request error: {0}")]
    RequestError(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),

    #[cfg(feature = "browser")]
    #[error("Cookie not found: {0}")]
    CookieNotFound(String),

    #[error("API error (status {status}): {body}")]
    ApiError { status: u16, body: String },
}

pub type Result<T> = std::result::Result<T, ClaudeApiError>;
