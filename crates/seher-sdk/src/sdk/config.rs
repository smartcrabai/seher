//! YAML config types for the Seher SDK (`providers` map).
//!
//! Mirrors `seher-ts/packages/sdk/src/types.ts` and the validator in `validate.ts`.

use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::sdk::{is_client_error_retryable, is_transient_http_error};

/// Per-provider API config forwarded to the underlying SDK constructor.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderApi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// Opt-in / opt-out flags for skill auto-discovery.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillsConfig {
    #[serde(
        default,
        rename = "includeClaude",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_claude: Option<bool>,
}

/// Skills config with all fields resolved to concrete values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSkillsConfig {
    pub include_claude: bool,
}

impl Default for ResolvedSkillsConfig {
    fn default() -> Self {
        Self {
            include_claude: true,
        }
    }
}

/// Retry policy configuration.
///
/// Provider-level settings override root-level settings; missing values fall
/// back to the defaults below.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetryConfig {
    #[serde(default = "RetryConfig::default_enabled", rename = "enabled")]
    pub enabled: bool,
    #[serde(default = "RetryConfig::default_max_attempts", rename = "maxAttempts")]
    pub max_attempts: u32,
    #[serde(
        default = "RetryConfig::default_initial_delay_secs",
        rename = "initialDelaySecs"
    )]
    pub initial_delay_secs: u64,
    #[serde(
        default = "RetryConfig::default_max_delay_secs",
        rename = "maxDelaySecs"
    )]
    pub max_delay_secs: u64,
    #[serde(default = "RetryConfig::default_multiplier", rename = "multiplier")]
    pub multiplier: f64,
    #[serde(
        default = "RetryConfig::default_retry_client_errors",
        rename = "retryClientErrors"
    )]
    pub retry_client_errors: bool,
}

impl RetryConfig {
    fn default_enabled() -> bool {
        true
    }

    fn default_max_attempts() -> u32 {
        5
    }

    fn default_initial_delay_secs() -> u64 {
        2
    }

    fn default_max_delay_secs() -> u64 {
        60
    }

    fn default_multiplier() -> f64 {
        2.0
    }

    fn default_retry_client_errors() -> bool {
        false
    }

    /// Return a sane `max_attempts` value even when a user bypasses schema
    /// validation and supplies `0`.
    #[must_use]
    pub fn effective_max_attempts(&self) -> u32 {
        self.max_attempts.max(1)
    }

    /// Whether a free-form error message describes a failure that should be
    /// retried under this policy.
    #[must_use]
    pub fn is_retryable_message(&self, message: &str) -> bool {
        is_transient_http_error(message)
            || (self.retry_client_errors && is_client_error_retryable(message))
    }

    /// Return a sane multiplier value even when a user bypasses schema
    /// validation and supplies a value below `1.0`. Values below `1.0` would
    /// decay the delay instead of backing off, so we clamp to `1.0`.
    #[must_use]
    pub fn effective_multiplier(&self) -> f64 {
        self.multiplier.max(1.0)
    }

    /// Compute the backoff delay for a given 1-based attempt number.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "delay values are small configuration integers; loss/truncation is acceptable"
    )]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let exponent = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
        let delay_secs =
            self.initial_delay_secs as f64 * self.effective_multiplier().powi(exponent);
        let clamped = delay_secs.min(self.max_delay_secs as f64) as u64;
        Duration::from_secs(clamped)
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            max_attempts: Self::default_max_attempts(),
            initial_delay_secs: Self::default_initial_delay_secs(),
            max_delay_secs: Self::default_max_delay_secs(),
            multiplier: Self::default_multiplier(),
            retry_client_errors: Self::default_retry_client_errors(),
        }
    }
}

/// Per-mode model entry inside a [`ProviderEntry`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelEntry {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// Raw model entry: either a bare string (model id) or a full struct.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum ModelEntryRaw {
    Bare(String),
    Full(ModelEntry),
}

impl From<ModelEntryRaw> for ModelEntry {
    fn from(raw: ModelEntryRaw) -> Self {
        match raw {
            ModelEntryRaw::Bare(model) => Self {
                model,
                priority: None,
            },
            ModelEntryRaw::Full(m) => m,
        }
    }
}

/// Raw provider entry parsed from YAML before normalization.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProviderEntryRaw {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub api: Option<ProviderApi>,
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    pub models: IndexMap<String, ModelEntryRaw>,
}

/// A single provider in the YAML `providers` map (after normalization).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderEntry {
    /// YAML map key as written in the config (stable label).
    pub key: String,
    /// Insertion order in the original YAML map (for stable tiebreaks).
    pub order: usize,
    /// Resolved provider name. Equals the explicit `provider` field when specified
    /// in YAML, otherwise falls back to `key`.
    pub provider: String,
    /// Underlying SDK kind (e.g. `"pi"`, `"claude"`, `"claude-terminal"`,
    /// `"claude-headless"`). Defaults to `"pi"` when omitted in YAML; kept as a
    /// string for forward compatibility.
    pub sdk: String,
    /// Provider-level priority shorthand.
    pub priority: Option<i32>,
    pub api: Option<ProviderApi>,
    pub skills: Option<SkillsConfig>,
    /// Provider-level retry policy override.
    pub retry: Option<RetryConfig>,
    /// Mode -> model entry. Keys include `plan`, `build`, plus user-defined keys.
    pub models: IndexMap<String, ModelEntry>,
}

/// Raw root config from YAML.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ConfigRaw {
    #[serde(default)]
    pub providers: IndexMap<String, ProviderEntryRaw>,
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
}

/// Normalized config root.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub providers: Vec<ProviderEntry>,
    pub skills: Option<SkillsConfig>,
    pub retry: Option<RetryConfig>,
}

impl Config {
    /// Resolve effective skills config for a provider entry, falling back to root,
    /// then to defaults (`include_claude = true`).
    #[must_use]
    pub fn resolve_skills(&self, entry: &ProviderEntry) -> ResolvedSkillsConfig {
        ResolvedSkillsConfig {
            include_claude: entry
                .skills
                .as_ref()
                .and_then(|s| s.include_claude)
                .or_else(|| self.skills.as_ref().and_then(|s| s.include_claude))
                .unwrap_or(true),
        }
    }

    /// Resolve effective retry config for a provider entry, falling back to root,
    /// then to hard-coded defaults.
    #[must_use]
    pub fn resolve_retry(&self, entry: &ProviderEntry) -> RetryConfig {
        match (&entry.retry, &self.retry) {
            (Some(provider_retry), _) => provider_retry.clone(),
            (None, Some(root_retry)) => root_retry.clone(),
            (None, None) => RetryConfig::default(),
        }
    }
}

impl From<ConfigRaw> for Config {
    fn from(raw: ConfigRaw) -> Self {
        let providers = raw
            .providers
            .into_iter()
            .enumerate()
            .map(|(order, (key, p))| {
                let provider = p.provider.unwrap_or_else(|| key.clone());
                let sdk = p.sdk.unwrap_or_else(|| "pi".to_string());
                let models: IndexMap<String, ModelEntry> =
                    p.models.into_iter().map(|(k, v)| (k, v.into())).collect();
                ProviderEntry {
                    key,
                    order,
                    provider,
                    sdk,
                    priority: p.priority,
                    api: p.api,
                    skills: p.skills,
                    retry: p.retry,
                    models,
                }
            })
            .collect();
        Self {
            providers,
            skills: raw.skills,
            retry: raw.retry,
        }
    }
}

/// Output of [`resolve_agent`](crate::sdk::resolve::resolve_agent): which provider/model to use.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAgent {
    /// Resolved provider name (e.g., "claude", "zai").
    pub provider: String,
    /// Concrete model id passed to pi (e.g. "anthropic/claude-sonnet-4-5").
    pub model_id: String,
    /// Mode key used during resolution (plan / build / custom).
    pub mode_key: String,
    /// SDK kind (e.g. "pi", "claude-terminal").
    pub sdk: String,
    /// API config to forward.
    pub api: Option<ProviderApi>,
    /// Skill discovery flags resolved from per-provider > root > defaults.
    pub skills: ResolvedSkillsConfig,
    /// Retry policy resolved from per-provider > root > defaults.
    pub retry: RetryConfig,
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    #[test]
    fn bare_model_string_parses_as_model_entry() {
        let raw: ModelEntryRaw = serde_yaml::from_str("opus-4.7").expect("parse");
        let entry: ModelEntry = raw.into();
        assert_eq!(entry.model, "opus-4.7");
        assert_eq!(entry.priority, None);
    }

    #[test]
    fn full_model_entry_parses_with_priority() {
        let raw: ModelEntryRaw =
            serde_yaml::from_str("{ model: opus-4.7, priority: 5 }").expect("parse");
        let entry: ModelEntry = raw.into();
        assert_eq!(entry.model, "opus-4.7");
        assert_eq!(entry.priority, Some(5));
    }

    #[test]
    fn provider_key_defaults_to_provider_name() {
        let yaml = "
providers:
  claude:
    models:
      build: opus-4.7
";
        let raw: ConfigRaw = serde_yaml::from_str(yaml).expect("parse");
        let cfg: Config = raw.into();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].key, "claude");
        assert_eq!(cfg.providers[0].provider, "claude");
        assert_eq!(cfg.providers[0].sdk, "pi");
    }

    #[test]
    fn explicit_provider_overrides_key() {
        let yaml = "
providers:
  zai:
    provider: zai
    sdk: pi
    api:
      key: sk-test
      endpoint: https://api.zai.example.com
    models:
      build: zai/glm-5.1
";
        let raw: ConfigRaw = serde_yaml::from_str(yaml).expect("parse");
        let cfg: Config = raw.into();
        let zai = &cfg.providers[0];
        assert_eq!(zai.key, "zai");
        assert_eq!(zai.provider, "zai");
        assert_eq!(
            zai.api.as_ref().and_then(|a| a.key.as_deref()),
            Some("sk-test")
        );
        assert_eq!(
            zai.api.as_ref().and_then(|a| a.endpoint.as_deref()),
            Some("https://api.zai.example.com"),
        );
        assert_eq!(zai.models["build"].model, "zai/glm-5.1");
    }

    #[test]
    fn provider_order_is_preserved() {
        let yaml = "
providers:
  codex:
    models: { build: gpt-5.5 }
  claude:
    models: { build: opus-4.7 }
  cursor:
    models: { build: cursor/some-model }
";
        let raw: ConfigRaw = serde_yaml::from_str(yaml).expect("parse");
        let cfg: Config = raw.into();
        let keys: Vec<&str> = cfg.providers.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["codex", "claude", "cursor"]);
        let orders: Vec<usize> = cfg.providers.iter().map(|p| p.order).collect();
        assert_eq!(orders, vec![0, 1, 2]);
    }

    #[test]
    fn skills_resolution_falls_through_to_default() {
        let cfg = Config::default();
        let entry = ProviderEntry {
            key: "x".into(),
            order: 0,
            provider: "x".into(),
            sdk: "pi".into(),
            priority: None,
            api: None,
            skills: None,
            retry: None,
            models: IndexMap::new(),
        };
        assert!(cfg.resolve_skills(&entry).include_claude);
    }

    #[test]
    fn skills_resolution_per_provider_overrides_root() {
        let cfg = Config {
            providers: vec![],
            skills: Some(SkillsConfig {
                include_claude: Some(false),
            }),
            retry: None,
        };
        let entry = ProviderEntry {
            key: "x".into(),
            order: 0,
            provider: "x".into(),
            sdk: "pi".into(),
            priority: None,
            api: None,
            skills: Some(SkillsConfig {
                include_claude: Some(true),
            }),
            retry: None,
            models: IndexMap::new(),
        };
        assert!(cfg.resolve_skills(&entry).include_claude);
    }

    // -----------------------------------------------------------------------
    // RetryConfig parsing and resolution
    // -----------------------------------------------------------------------

    #[test]
    fn retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_attempts, 5);
        assert_eq!(cfg.initial_delay_secs, 2);
        assert_eq!(cfg.max_delay_secs, 60);
        assert!((cfg.multiplier - 2.0).abs() < f64::EPSILON);
        assert!(!cfg.retry_client_errors);
    }

    #[test]
    fn retry_config_parses_camel_case_yaml() {
        let yaml = "
enabled: false
maxAttempts: 3
initialDelaySecs: 1
maxDelaySecs: 10
multiplier: 1.5
retryClientErrors: true
";
        let parsed: RetryConfig = serde_yaml::from_str(yaml).expect("parse");
        assert!(!parsed.enabled);
        assert_eq!(parsed.max_attempts, 3);
        assert_eq!(parsed.initial_delay_secs, 1);
        assert_eq!(parsed.max_delay_secs, 10);
        assert!((parsed.multiplier - 1.5).abs() < f64::EPSILON);
        assert!(parsed.retry_client_errors);
    }

    #[test]
    fn retry_config_partial_yaml_uses_defaults_for_missing_fields() {
        let yaml = "maxAttempts: 2";
        let parsed: RetryConfig = serde_yaml::from_str(yaml).expect("parse");
        assert!(parsed.enabled);
        assert_eq!(parsed.max_attempts, 2);
        assert!(!parsed.retry_client_errors);
    }

    #[test]
    fn retry_resolution_uses_defaults_when_no_config() {
        let cfg = Config::default();
        let entry = ProviderEntry {
            key: "x".into(),
            order: 0,
            provider: "x".into(),
            sdk: "pi".into(),
            priority: None,
            api: None,
            skills: None,
            retry: None,
            models: IndexMap::new(),
        };
        let resolved = cfg.resolve_retry(&entry);
        assert!(resolved.enabled);
        assert_eq!(resolved.max_attempts, 5);
        assert!(!resolved.retry_client_errors);
    }

    #[test]
    fn retry_resolution_root_overrides_defaults() {
        let cfg = Config {
            providers: vec![],
            skills: None,
            retry: Some(RetryConfig {
                enabled: false,
                ..RetryConfig::default()
            }),
        };
        let entry = ProviderEntry {
            key: "x".into(),
            order: 0,
            provider: "x".into(),
            sdk: "pi".into(),
            priority: None,
            api: None,
            skills: None,
            retry: None,
            models: IndexMap::new(),
        };
        assert!(!cfg.resolve_retry(&entry).enabled);
    }

    #[test]
    fn retry_resolution_provider_overrides_root() {
        let cfg = Config {
            providers: vec![],
            skills: None,
            retry: Some(RetryConfig {
                enabled: false,
                ..RetryConfig::default()
            }),
        };
        let entry = ProviderEntry {
            key: "x".into(),
            order: 0,
            provider: "x".into(),
            sdk: "pi".into(),
            priority: None,
            api: None,
            skills: None,
            retry: Some(RetryConfig {
                enabled: true,
                retry_client_errors: true,
                ..RetryConfig::default()
            }),
            models: IndexMap::new(),
        };
        let resolved = cfg.resolve_retry(&entry);
        assert!(resolved.enabled);
        assert!(resolved.retry_client_errors);
    }

    #[test]
    fn retry_resolution_provider_replaces_root_entirely() {
        // Per the design, RetryConfig is treated as a whole Option<RetryConfig>;
        // a provider-level override does NOT merge individual fields with root.
        let cfg = Config {
            providers: vec![],
            skills: None,
            retry: Some(RetryConfig {
                max_attempts: 3,
                ..RetryConfig::default()
            }),
        };
        let entry = ProviderEntry {
            key: "x".into(),
            order: 0,
            provider: "x".into(),
            sdk: "pi".into(),
            priority: None,
            api: None,
            skills: None,
            retry: Some(RetryConfig {
                enabled: false,
                ..RetryConfig::default()
            }),
            models: IndexMap::new(),
        };
        let resolved = cfg.resolve_retry(&entry);
        assert!(!resolved.enabled);
        // max_attempts comes from provider defaults, NOT from root's 3.
        assert_eq!(resolved.max_attempts, 5);
    }

    #[test]
    fn retry_config_roundtrips_through_yaml() {
        let original = RetryConfig {
            enabled: true,
            max_attempts: 7,
            initial_delay_secs: 3,
            max_delay_secs: 120,
            multiplier: 3.0,
            retry_client_errors: true,
        };
        let yaml = serde_yaml::to_string(&original).expect("serialize");
        let parsed: RetryConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(parsed.enabled, original.enabled);
        assert_eq!(parsed.max_attempts, original.max_attempts);
        assert_eq!(parsed.initial_delay_secs, original.initial_delay_secs);
        assert_eq!(parsed.max_delay_secs, original.max_delay_secs);
        assert!(
            (parsed.multiplier - original.multiplier).abs() < f64::EPSILON,
            "multiplier should round-trip exactly: got {}, expected {}",
            parsed.multiplier,
            original.multiplier
        );
        assert_eq!(parsed.retry_client_errors, original.retry_client_errors);
    }

    #[test]
    fn retry_config_multiplier_below_one_is_clamped() {
        let cfg = RetryConfig {
            multiplier: 0.5,
            ..RetryConfig::default()
        };
        assert!((cfg.effective_multiplier() - 1.0).abs() < f64::EPSILON);
        // With an initial delay of 2s, attempts 1..=3 should all be 2s instead
        // of decaying to 1s / 0.5s.
        assert_eq!(cfg.delay_for_attempt(1).as_secs(), 2);
        assert_eq!(cfg.delay_for_attempt(2).as_secs(), 2);
        assert_eq!(cfg.delay_for_attempt(3).as_secs(), 2);
    }
}
