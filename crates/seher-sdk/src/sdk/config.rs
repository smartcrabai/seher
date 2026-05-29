//! YAML config types for the Seher SDK (`providers` map).
//!
//! Mirrors `seher-ts/packages/sdk/src/types.ts` and the validator in `validate.ts`.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

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
    pub models: IndexMap<String, ModelEntryRaw>,
}

/// A single provider in the YAML `providers` map (after normalization).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    /// YAML map key as written in the config (stable label).
    pub key: String,
    /// Insertion order in the original YAML map (for stable tiebreaks).
    pub order: usize,
    /// Resolved provider name. Equals the explicit `provider` field when specified
    /// in YAML, otherwise falls back to `key`.
    pub provider: String,
    /// Underlying SDK kind. Always `"pi"` in this implementation (pi is the only
    /// execution engine); kept as a string for forward compatibility.
    pub sdk: String,
    /// Provider-level priority shorthand.
    pub priority: Option<i32>,
    pub api: Option<ProviderApi>,
    pub skills: Option<SkillsConfig>,
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
}

/// Normalized config root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub providers: Vec<ProviderEntry>,
    pub skills: Option<SkillsConfig>,
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
                    models,
                }
            })
            .collect();
        Self {
            providers,
            skills: raw.skills,
        }
    }
}

/// Output of [`resolve_agent`](crate::sdk::resolve::resolve_agent): which provider/model to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// Resolved provider name (e.g., "claude", "zai").
    pub provider: String,
    /// Concrete model id passed to pi (e.g. "anthropic/claude-sonnet-4-5").
    pub model_id: String,
    /// Mode key used during resolution (plan / build / custom).
    pub mode_key: String,
    /// API config to forward.
    pub api: Option<ProviderApi>,
    /// Skill discovery flags resolved from per-provider > root > defaults.
    pub skills: ResolvedSkillsConfig,
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
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
            models: IndexMap::new(),
        };
        assert_eq!(cfg.resolve_skills(&entry).include_claude, true);
    }

    #[test]
    fn skills_resolution_per_provider_overrides_root() {
        let cfg = Config {
            providers: vec![],
            skills: Some(SkillsConfig {
                include_claude: Some(false),
            }),
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
            models: IndexMap::new(),
        };
        assert_eq!(cfg.resolve_skills(&entry).include_claude, true);
    }
}
