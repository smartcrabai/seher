//! Errors raised while invoking the external `codexbar` binary.
//!
//! Mirrors `seher-ts/packages/sdk/src/codexbar/errors.ts`. The resolver treats
//! every variant as "provider is available" so a missing/erroring codexbar never
//! permanently drops a provider from the candidate list.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodexBarError {
    #[error(
        "codexbar binary not found at {bin}. Install CodexBar or set bin_path/SEHER_CODEXBAR_BIN."
    )]
    NotFound { bin: String },

    #[error("failed to spawn codexbar: {0}")]
    Spawn(String),

    #[error("codexbar usage timed out after {ms}ms (provider={provider})")]
    Timeout { provider: String, ms: u128 },

    #[error("codexbar usage exited with code {code:?} (provider={provider}): {stderr}")]
    Exited {
        code: Option<i32>,
        provider: String,
        stderr: String,
    },

    #[error("failed to parse codexbar JSON output: {0}")]
    Parse(String),

    #[error("codexbar returned a non-array JSON payload (provider={0})")]
    NonArray(String),

    #[error("codexbar returned no entry for provider={0}")]
    NoEntry(String),
}
