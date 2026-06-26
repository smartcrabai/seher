//! Error types mirroring `claude_agent_sdk._errors`.

use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ClaudeSDKError>;

#[derive(Debug, Error)]
pub enum ClaudeSDKError {
    #[error("claude CLI not found{}", path.as_deref().map(|p| format!(": {p}")).unwrap_or_default())]
    CliNotFound { path: Option<String> },

    #[error("CLI connection error: {0}")]
    Connection(String),

    #[error(
        "CLI process failed{}{}",
        exit_code.map(|c| format!(" (exit code: {c})")).unwrap_or_default(),
        stderr.as_deref().map(|s| format!("\nError output: {s}")).unwrap_or_default()
    )]
    Process {
        message: String,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },

    #[error("failed to decode JSON: {snippet}")]
    JsonDecode {
        snippet: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to parse message: {0}")]
    MessageParse(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl ClaudeSDKError {
    pub(crate) fn connection(msg: impl Into<String>) -> Self {
        Self::Connection(msg.into())
    }

    /// Helper constructor for [`Self::Process`].
    #[must_use]
    pub fn process(
        message: impl Into<String>,
        exit_code: Option<i32>,
        stderr: Option<String>,
    ) -> Self {
        Self::Process {
            message: message.into(),
            exit_code,
            stderr,
        }
    }

    pub(crate) fn json_decode(line: &str, err: serde_json::Error) -> Self {
        // Truncate on a UTF-8 char boundary; slicing on a raw byte index would
        // panic if the 100th byte sits inside a multibyte codepoint
        // (Japanese, emoji, accented Latin, ...).
        let snippet = if line.len() > 100 {
            let cut = line
                .char_indices()
                .take_while(|(i, _)| *i < 100)
                .last()
                .map_or(0, |(i, c)| i + c.len_utf8());
            format!("{}...", &line[..cut])
        } else {
            line.to_string()
        };
        Self::JsonDecode {
            snippet,
            source: err,
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    fn fake_serde_err() -> serde_json::Error {
        serde_json::from_str::<serde_json::Value>("not json").unwrap_err()
    }

    #[test]
    fn json_decode_truncates_short_lines_intact() {
        let e = ClaudeSDKError::json_decode("hi", fake_serde_err());
        match e {
            ClaudeSDKError::JsonDecode { snippet, .. } => assert_eq!(snippet, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn json_decode_truncates_long_ascii_with_ellipsis() {
        let long = "x".repeat(150);
        let e = ClaudeSDKError::json_decode(&long, fake_serde_err());
        match e {
            ClaudeSDKError::JsonDecode { snippet, .. } => {
                assert!(snippet.ends_with("..."));
                // 100 'x' bytes + "..." = 103 chars.
                assert_eq!(snippet.len(), 103);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn json_decode_does_not_panic_on_multibyte_boundary() {
        // Each U+3042 is 3 bytes; 40 of them = 120 bytes -- the cut at byte 100
        // would land mid-codepoint with a naive slice. The safe version must
        // round down to the previous char boundary.
        let long: String = "\u{3042}".repeat(40);
        let e = ClaudeSDKError::json_decode(&long, fake_serde_err());
        let ClaudeSDKError::JsonDecode { snippet, .. } = e else {
            panic!("wrong variant")
        };
        assert!(snippet.ends_with("..."));
        // Body must be a valid prefix made of whole U+3042 codepoints (<= 99 bytes
        // = 33 chars x 3 bytes).
        let body = snippet.trim_end_matches("...");
        assert!(body.len() % 3 == 0, "len={}", body.len());
        assert!(body.chars().all(|c| c == '\u{3042}'));
    }
}
