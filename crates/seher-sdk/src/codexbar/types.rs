//! Deserialization types for the `codexbar usage --format json` payload.
//!
//! Mirrors `seher-ts/packages/sdk/src/codexbar/types.ts`. Unknown fields are
//! ignored so the structs stay forward-compatible with new codexbar releases.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexBarWindow {
    #[serde(default)]
    pub used_percent: f64,
    #[serde(default)]
    pub window_minutes: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub reset_description: Option<String>,
    #[serde(default)]
    pub next_regen_percent: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedCodexBarWindow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub window: CodexBarWindow,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexBarUsage {
    #[serde(default)]
    pub primary: Option<CodexBarWindow>,
    #[serde(default)]
    pub secondary: Option<CodexBarWindow>,
    #[serde(default)]
    pub tertiary: Option<CodexBarWindow>,
    #[serde(default)]
    pub extra_rate_windows: Option<Vec<NamedCodexBarWindow>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexBarUsageResponse {
    pub provider: String,
    #[serde(default)]
    pub usage: CodexBarUsage,
}
