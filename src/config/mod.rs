use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub models: Option<HashMap<String, String>>,
    #[serde(default)]
    pub arg_maps: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            agents: vec![AgentConfig {
                command: "claude".to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
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

        assert_eq!(settings.agents.len(), 2);
    }

    #[test]
    fn test_sample_settings_claude_agent() {
        let content = std::fs::read_to_string(sample_settings_path()).unwrap();
        let settings: Settings = serde_json::from_str(&content).unwrap();

        let claude = &settings.agents[0];
        assert_eq!(claude.command, "claude");
        assert_eq!(
            claude.args,
            ["--model", "{model}"]
        );

        let models = claude.models.as_ref().expect("models should be present");
        assert_eq!(models.get("high").map(String::as_str), Some("opus"));
        assert_eq!(models.get("low").map(String::as_str), Some("sonnet"));
        assert_eq!(
            claude.arg_maps.get("--danger").cloned(),
            Some(vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
            ])
        );
    }

    #[test]
    fn test_sample_settings_copilot_agent() {
        let content = std::fs::read_to_string(sample_settings_path()).unwrap();
        let settings: Settings = serde_json::from_str(&content).unwrap();

        let copilot = &settings.agents[1];
        assert_eq!(copilot.command, "copilot");
        assert_eq!(copilot.args, ["--model", "{model}"]);

        let models = copilot.models.as_ref().expect("models should be present");
        assert_eq!(
            models.get("high").map(String::as_str),
            Some("claude-opus-4.5")
        );
        assert_eq!(
            models.get("low").map(String::as_str),
            Some("claude-sonnet-4.5")
        );
        assert_eq!(
            copilot.arg_maps.get("--danger").cloned(),
            Some(vec!["--yolo".to_string()])
        );
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
        assert!(settings.agents[0].arg_maps.is_empty());
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
        assert!(settings.agents[0].arg_maps.is_empty());
    }

    #[test]
    fn test_parse_settings_with_arg_maps() {
        let json = r#"{"agents": [{"command": "claude", "arg_maps": {"--danger": ["--permission-mode", "bypassPermissions"]}}]}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(
            settings.agents[0].arg_maps.get("--danger").cloned(),
            Some(vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
            ])
        );
    }
}
