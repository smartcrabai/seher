use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub agents: Vec<AgentConfig>,
}

fn deserialize_provider<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    Ok(Some(opt))
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub models: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_provider")]
    pub provider: Option<Option<String>>,
}

fn provider_to_domain(provider: &str) -> Option<&str> {
    match provider {
        "claude" => Some("claude.ai"),
        "copilot" => Some("github.com"),
        _ => None,
    }
}

impl AgentConfig {
    pub fn resolve_domain(&self) -> Option<&str> {
        match &self.provider {
            Some(Some(p)) => provider_to_domain(p),
            Some(None) => None,
            None => provider_to_domain(&self.command),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            agents: vec![AgentConfig {
                command: "claude".to_string(),
                args: vec![],
                models: None,
                env: None,
                provider: None,
            }],
        }
    }
}

impl Settings {
    pub fn load(path: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::settings_path()?,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(e) => return Err(e.into()),
        };
        let settings: Settings = serde_json::from_str(&content)?;
        Ok(settings)
    }

    fn settings_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let home = dirs::home_dir().ok_or("HOME directory not found")?;
        Ok(home.join(".seher").join("settings.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_settings_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("settings.json")
    }

    #[test]
    fn test_parse_sample_settings() {
        let content = std::fs::read_to_string(sample_settings_path())
            .expect("examples/settings.json not found");
        let settings: Settings = serde_json::from_str(&content).expect("failed to parse settings");

        assert_eq!(settings.agents.len(), 3);
    }

    #[test]
    fn test_sample_settings_claude_agent() {
        let content = std::fs::read_to_string(sample_settings_path()).unwrap();
        let settings: Settings = serde_json::from_str(&content).unwrap();

        let claude = &settings.agents[0];
        assert_eq!(claude.command, "claude");
        assert_eq!(claude.args, ["--model", "{model}"]);

        let models = claude.models.as_ref().expect("models should be present");
        assert_eq!(models.get("high").map(String::as_str), Some("opus"));
        assert_eq!(models.get("medium").map(String::as_str), Some("sonnet"));

        // no provider field → None (inferred from command name)
        assert!(claude.provider.is_none());
        assert_eq!(claude.resolve_domain(), Some("claude.ai"));
    }

    #[test]
    fn test_sample_settings_copilot_agent() {
        let content = std::fs::read_to_string(sample_settings_path()).unwrap();
        let settings: Settings = serde_json::from_str(&content).unwrap();

        let opencode = &settings.agents[1];
        assert_eq!(opencode.command, "opencode");
        assert_eq!(opencode.args, ["--model", "{model}", "--yolo"]);

        let models = opencode.models.as_ref().expect("models should be present");
        assert_eq!(
            models.get("high").map(String::as_str),
            Some("github-copilot/gpt-5.4")
        );
        assert_eq!(
            models.get("low").map(String::as_str),
            Some("github-copilot/claude-haiku-4.5")
        );

        // provider: "copilot" → Some(Some("copilot"))
        assert_eq!(opencode.provider, Some(Some("copilot".to_string())));
        assert_eq!(opencode.resolve_domain(), Some("github.com"));
    }

    #[test]
    fn test_sample_settings_fallback_agent() {
        let content = std::fs::read_to_string(sample_settings_path()).unwrap();
        let settings: Settings = serde_json::from_str(&content).unwrap();

        let fallback = &settings.agents[2];
        assert_eq!(fallback.command, "claude");

        // provider: null → Some(None) (fallback)
        assert_eq!(fallback.provider, Some(None));
        assert_eq!(fallback.resolve_domain(), None);
    }

    #[test]
    fn test_provider_field_absent() {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert!(settings.agents[0].provider.is_none());
        assert_eq!(settings.agents[0].resolve_domain(), Some("claude.ai"));
    }

    #[test]
    fn test_provider_field_null() {
        let json = r#"{"agents": [{"command": "claude", "provider": null}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(settings.agents[0].provider, Some(None));
        assert_eq!(settings.agents[0].resolve_domain(), None);
    }

    #[test]
    fn test_provider_field_string() {
        let json = r#"{"agents": [{"command": "opencode", "provider": "copilot"}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(
            settings.agents[0].provider,
            Some(Some("copilot".to_string()))
        );
        assert_eq!(settings.agents[0].resolve_domain(), Some("github.com"));
    }

    #[test]
    fn test_provider_unknown_string() {
        let json = r#"{"agents": [{"command": "someai", "provider": "unknown"}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(
            settings.agents[0].provider,
            Some(Some("unknown".to_string()))
        );
        assert_eq!(settings.agents[0].resolve_domain(), None);
    }

    #[test]
    fn test_parse_minimal_settings_without_models() {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings =
            serde_json::from_str(json).expect("failed to parse minimal settings");

        assert_eq!(settings.agents.len(), 1);
        assert_eq!(settings.agents[0].command, "claude");
        assert!(settings.agents[0].args.is_empty());
        assert!(settings.agents[0].models.is_none());
    }

    #[test]
    fn test_parse_settings_with_env() {
        let json = r#"{"agents": [{"command": "claude", "env": {"ANTHROPIC_API_KEY": "sk-test", "CLAUDE_CODE_MAX_TURNS": "100"}}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        let env = settings.agents[0]
            .env
            .as_ref()
            .expect("env should be present");
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(
            env.get("CLAUDE_CODE_MAX_TURNS").map(String::as_str),
            Some("100")
        );
    }

    #[test]
    fn test_parse_settings_with_args_no_models() {
        let json = r#"{"agents": [{"command": "claude", "args": ["--permission-mode", "bypassPermissions"]}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(
            settings.agents[0].args,
            ["--permission-mode", "bypassPermissions"]
        );
        assert!(settings.agents[0].models.is_none());
    }
}
