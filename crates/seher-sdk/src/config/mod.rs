//! Minimal provider config types used as the bridge into [`crate::agent::Agent`].
//!
//! The resolver synthesizes an [`AgentConfig`] on the fly so the existing
//! cookie-based limit checkers in [`crate::agent::Agent::check_limit`] keep
//! working unchanged. This module therefore exposes only the minimal shape
//! required by that dispatch.

use std::collections::HashMap;

/// Provider field state:
/// - `None` (`Option::None`) → infer provider from the command name
/// - `Some(Explicit(name))` → use that provider name
/// - `Some(Null)` → explicitly no provider (cookie-less fallback)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConfig {
    Explicit(String),
    /// Maps to the YAML `provider: null` case — no provider, no cookie check.
    Null,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub command: String,
    pub env: Option<HashMap<String, String>>,
    pub provider: Option<ProviderConfig>,
    pub openrouter_management_key: Option<String>,
    pub glm_api_key: Option<String>,
}

fn command_to_provider(command: &str) -> Option<&str> {
    match command {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "copilot" => Some("copilot"),
        "glm" => Some("glm"),
        "zai" => Some("zai"),
        "kimi-k2" => Some("kimi-k2"),
        "warp" => Some("warp"),
        "kiro" => Some("kiro"),
        _ => None,
    }
}

fn resolve_provider<'a>(command: &'a str, provider: Option<&'a ProviderConfig>) -> Option<&'a str> {
    match provider {
        Some(ProviderConfig::Explicit(name)) => Some(name.as_str()),
        Some(ProviderConfig::Null) => None,
        None => command_to_provider(command),
    }
}

impl AgentConfig {
    #[must_use]
    pub fn resolve_provider(&self) -> Option<&str> {
        resolve_provider(&self.command, self.provider.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_provider_passes_through() {
        let cfg = AgentConfig {
            command: "myai".to_string(),
            env: None,
            provider: Some(ProviderConfig::Explicit("copilot".to_string())),
            openrouter_management_key: None,
            glm_api_key: None,
        };
        assert_eq!(cfg.resolve_provider(), Some("copilot"));
    }

    #[test]
    fn null_provider_returns_none() {
        let cfg = AgentConfig {
            command: "claude".to_string(),
            env: None,
            provider: Some(ProviderConfig::Null),
            openrouter_management_key: None,
            glm_api_key: None,
        };
        assert_eq!(cfg.resolve_provider(), None);
    }

    #[test]
    fn inferred_provider_falls_back_to_command() {
        let cfg = AgentConfig {
            command: "claude".to_string(),
            env: None,
            provider: None,
            openrouter_management_key: None,
            glm_api_key: None,
        };
        assert_eq!(cfg.resolve_provider(), Some("claude"));
    }
}
