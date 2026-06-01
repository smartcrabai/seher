//! Limit determination backed by the external [`codexbar`](https://codexbar.app/)
//! binary.
//!
//! Port of `seher-ts/packages/sdk/src/codexbar/`. Instead of fetching usage from
//! each provider's API with browser cookies, we shell out to `codexbar usage
//! --format json --provider <provider>` and classify the returned rate windows.

mod client;
mod errors;
mod limit;
pub mod types;

pub use client::{RunCodexBarUsageOptions, run_codexbar_usage};
pub use errors::CodexBarError;
pub use limit::{AgentLimit, check_limit, check_limit_with};
pub use types::{CodexBarUsage, CodexBarUsageResponse, CodexBarWindow, NamedCodexBarWindow};
