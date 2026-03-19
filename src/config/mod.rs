use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priority: Vec<PriorityRule>,
    pub agents: Vec<AgentConfig>,
    #[serde(skip)]
    original_text: Option<String>,
}

/// Represents the three possible states of the `provider` field:
/// - `Inferred`: field absent → provider is inferred from the command name
/// - `Explicit(name)`: field has a string value → use that provider name
/// - `None`: field is `null` → no provider (fallback agent)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConfig {
    Inferred,
    Explicit(String),
    None,
}

impl<'de> serde::Deserialize<'de> for ProviderConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let opt: Option<String> = serde::Deserialize::deserialize(deserializer)?;
        Ok(match opt {
            Some(s) => ProviderConfig::Explicit(s),
            Option::None => ProviderConfig::None,
        })
    }
}

impl serde::Serialize for ProviderConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ProviderConfig::Explicit(s) => serializer.serialize_str(s),
            ProviderConfig::Inferred | ProviderConfig::None => serializer.serialize_none(),
        }
    }
}

fn deserialize_provider_config<'de, D>(deserializer: D) -> Result<Option<ProviderConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let config = ProviderConfig::deserialize(deserializer)?;
    Ok(Some(config))
}

#[expect(
    clippy::ref_option,
    reason = "&Option<T> is required by serde skip_serializing_if"
)]
fn is_inferred_or_absent_provider(value: &Option<ProviderConfig>) -> bool {
    matches!(value, Option::None | Some(ProviderConfig::Inferred))
}

#[expect(
    clippy::ref_option,
    reason = "&Option<T> is required by serde serialize_with"
)]
fn serialize_provider_config<S>(
    value: &Option<ProviderConfig>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(ProviderConfig::Explicit(s)) => serializer.serialize_str(s),
        Option::None | Some(ProviderConfig::Inferred | ProviderConfig::None) => {
            serializer.serialize_none()
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub arg_maps: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_provider_config",
        serialize_with = "serialize_provider_config",
        skip_serializing_if = "is_inferred_or_absent_provider"
    )]
    pub provider: Option<ProviderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrouter_management_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_command: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct PriorityRule {
    pub command: String,
    #[serde(
        default,
        deserialize_with = "deserialize_provider_config",
        serialize_with = "serialize_provider_config",
        skip_serializing_if = "is_inferred_or_absent_provider"
    )]
    pub provider: Option<ProviderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub priority: i32,
}

fn command_to_provider(command: &str) -> Option<&str> {
    match command {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "copilot" => Some("copilot"),
        _ => None,
    }
}

fn resolve_provider<'a>(command: &'a str, provider: Option<&'a ProviderConfig>) -> Option<&'a str> {
    match provider {
        Some(ProviderConfig::Explicit(name)) => Some(name.as_str()),
        Some(ProviderConfig::None) => Option::None,
        Some(ProviderConfig::Inferred) | Option::None => command_to_provider(command),
    }
}

fn provider_to_domain(provider: &str) -> Option<&str> {
    match provider {
        "claude" => Some("claude.ai"),
        "codex" => Some("chatgpt.com"),
        "copilot" => Some("github.com"),
        _ => None,
    }
}

impl AgentConfig {
    #[must_use]
    pub fn resolve_provider(&self) -> Option<&str> {
        resolve_provider(&self.command, self.provider.as_ref())
    }

    #[must_use]
    pub fn resolve_domain(&self) -> Option<&str> {
        self.resolve_provider().and_then(provider_to_domain)
    }
}

impl PriorityRule {
    #[must_use]
    pub fn resolve_provider(&self) -> Option<&str> {
        resolve_provider(&self.command, self.provider.as_ref())
    }

    #[must_use]
    pub fn matches(&self, command: &str, provider: Option<&str>, model: Option<&str>) -> bool {
        self.command == command
            && self.resolve_provider() == provider
            && self.model.as_deref() == model
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            priority: vec![],
            agents: vec![AgentConfig {
                command: "claude".to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
                provider: None,
                openrouter_management_key: None,
                pre_command: vec![],
            }],
            original_text: None,
        }
    }
}

fn serde_value_to_cst_input(val: &serde_json::Value) -> CstInputValue {
    match val {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(b) => CstInputValue::Bool(*b),
        serde_json::Value::Number(n) => CstInputValue::Number(n.to_string()),
        serde_json::Value::String(s) => CstInputValue::String(s.clone()),
        serde_json::Value::Array(arr) => {
            CstInputValue::Array(arr.iter().map(serde_value_to_cst_input).collect())
        }
        serde_json::Value::Object(obj) => CstInputValue::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), serde_value_to_cst_input(v)))
                .collect(),
        ),
    }
}

fn strip_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;

    while i < chars.len() {
        let c = chars[i];

        if in_string {
            result.push(c);
            if c == '\\' && i + 1 < chars.len() {
                i += 1;
                result.push(chars[i]);
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            result.push(c);
        } else if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == ']' || chars[j] == '}') {
                // trailing comma: skip it
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }

        i += 1;
    }

    result
}

impl Settings {
    #[must_use]
    pub fn priority_for(&self, agent: &AgentConfig, model: Option<&str>) -> i32 {
        self.priority_for_components(&agent.command, agent.resolve_provider(), model)
    }

    #[must_use]
    pub fn priority_for_components(
        &self,
        command: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> i32 {
        self.priority
            .iter()
            .find(|rule| rule.matches(command, provider, model))
            .map_or(0, |rule| rule.priority)
    }

    /// # Errors
    ///
    /// Returns an error if the settings file cannot be read or parsed.
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
        let mut stripped = json_comments::StripComments::new(content.as_bytes());
        let mut json_str = String::new();
        std::io::Read::read_to_string(&mut stripped, &mut json_str)?;
        let clean = strip_trailing_commas(&json_str);
        let mut settings: Settings = serde_json::from_str(&clean)?;
        settings.original_text = Some(content);
        Ok(settings)
    }

    fn save_with_cst(&self, original: &str) -> Result<String, Box<dyn std::error::Error>> {
        let root = CstRootNode::parse(original, &jsonc_parser::ParseOptions::default())
            .map_err(|e| e.to_string())?;
        let root_obj = root.object_value_or_set();

        let value = serde_json::to_value(self)?;
        let obj = value
            .as_object()
            .ok_or("settings serialized to non-object")?;

        for (key, val) in obj {
            let cst_input = serde_value_to_cst_input(val);
            if let Some(prop) = root_obj.get(key) {
                prop.set_value(cst_input);
            } else {
                root_obj.append(key, cst_input);
            }
        }

        let props_to_remove: Vec<_> = root_obj
            .properties()
            .into_iter()
            .filter(|prop| {
                prop.name()
                    .and_then(|n| n.decoded_value().ok())
                    .is_some_and(|name| !obj.contains_key(&name))
            })
            .collect();
        for prop in props_to_remove {
            prop.remove();
        }

        Ok(root.to_string())
    }

    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save(&self, path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::settings_path()?,
        };
        let output = match &self.original_text {
            Some(original) => self
                .save_with_cst(original)
                .or_else(|_| serde_json::to_string_pretty(self))?,
            None => serde_json::to_string_pretty(self)?,
        };
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut tmp, output.as_bytes())?;
        std::io::Write::flush(&mut tmp)?;
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Upsert a `PriorityRule`. If a matching rule (command + provider + model) already exists,
    /// its priority is updated. Otherwise a new rule is appended.
    pub fn upsert_priority(
        &mut self,
        command: &str,
        provider: Option<ProviderConfig>,
        model: Option<String>,
        priority: i32,
    ) {
        for rule in &mut self.priority {
            if rule.command == command && rule.provider == provider && rule.model == model {
                rule.priority = priority;
                return;
            }
        }
        self.priority.push(PriorityRule {
            command: command.to_string(),
            provider,
            model,
            priority,
        });
    }

    /// Remove a `PriorityRule` matching the given (command, provider, model) triple.
    pub fn remove_priority(
        &mut self,
        command: &str,
        provider: Option<&ProviderConfig>,
        model: Option<&str>,
    ) {
        self.priority.retain(|rule| {
            !(rule.command == command
                && rule.provider.as_ref() == provider
                && rule.model.as_deref() == model)
        });
    }

    fn settings_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let home = dirs::home_dir().ok_or("HOME directory not found")?;
        let dir = home.join(".config").join("seher");
        let jsonc_path = dir.join("settings.jsonc");
        if jsonc_path.exists() {
            return Ok(jsonc_path);
        }
        Ok(dir.join("settings.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_settings_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("settings.json")
    }

    fn load_sample() -> Result<Settings, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(sample_settings_path())?;
        let settings: Settings = serde_json::from_str(&content)?;
        Ok(settings)
    }

    #[test]
    fn test_parse_sample_settings() -> TestResult {
        let settings = load_sample()?;

        assert_eq!(settings.priority.len(), 4);
        assert_eq!(settings.agents.len(), 4);
        Ok(())
    }

    #[test]
    fn test_sample_settings_priority_rules() -> TestResult {
        let settings = load_sample()?;

        assert_eq!(
            settings.priority[0],
            PriorityRule {
                command: "opencode".to_string(),
                provider: Some(ProviderConfig::Explicit("copilot".to_string())),
                model: Some("high".to_string()),
                priority: 100,
            }
        );
        assert_eq!(
            settings.priority[2],
            PriorityRule {
                command: "claude".to_string(),
                provider: Some(ProviderConfig::None),
                model: Some("medium".to_string()),
                priority: 25,
            }
        );
        Ok(())
    }

    #[test]
    fn test_sample_settings_claude_agent() -> TestResult {
        let settings = load_sample()?;

        let claude = &settings.agents[0];
        assert_eq!(claude.command, "claude");
        assert_eq!(claude.args, ["--model", "{model}"]);

        let models = claude.models.as_ref();
        assert!(models.is_some());
        let models = models.ok_or("models should be present")?;
        assert_eq!(models.get("high").map(String::as_str), Some("opus"));
        assert_eq!(models.get("medium").map(String::as_str), Some("sonnet"));
        assert_eq!(
            claude.arg_maps.get("--danger").cloned(),
            Some(vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
            ])
        );

        // no provider field → None (inferred from command name)
        assert!(claude.provider.is_none());
        assert_eq!(claude.resolve_domain(), Some("claude.ai"));
        Ok(())
    }

    #[test]
    fn test_sample_settings_copilot_agent() -> TestResult {
        let settings = load_sample()?;

        let opencode = &settings.agents[1];
        assert_eq!(opencode.command, "opencode");
        assert_eq!(opencode.args, ["--model", "{model}", "--yolo"]);

        let models = opencode.models.as_ref().ok_or("models should be present")?;
        assert_eq!(
            models.get("high").map(String::as_str),
            Some("github-copilot/gpt-5.4")
        );
        assert_eq!(
            models.get("low").map(String::as_str),
            Some("github-copilot/claude-haiku-4.5")
        );

        // provider: "copilot" → Some(Explicit("copilot"))
        assert_eq!(
            opencode.provider,
            Some(ProviderConfig::Explicit("copilot".to_string()))
        );
        assert_eq!(opencode.resolve_domain(), Some("github.com"));
        Ok(())
    }

    #[test]
    fn test_sample_settings_fallback_agent() -> TestResult {
        let settings = load_sample()?;

        let fallback = &settings.agents[3];
        assert_eq!(fallback.command, "claude");

        // provider: null → Some(ProviderConfig::None) (fallback)
        assert_eq!(fallback.provider, Some(ProviderConfig::None));
        assert_eq!(fallback.resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_sample_settings_codex_agent() -> TestResult {
        let settings = load_sample()?;

        let codex = &settings.agents[2];
        assert_eq!(codex.command, "codex");
        assert!(codex.args.is_empty());
        assert!(codex.models.is_none());
        assert!(codex.provider.is_none());
        assert_eq!(codex.resolve_domain(), Some("chatgpt.com"));
        assert_eq!(codex.pre_command, ["git", "pull", "--rebase"]);
        Ok(())
    }

    #[test]
    fn test_provider_field_absent() -> TestResult {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert!(settings.agents[0].provider.is_none());
        assert_eq!(settings.agents[0].resolve_provider(), Some("claude"));
        assert_eq!(settings.agents[0].resolve_domain(), Some("claude.ai"));
        Ok(())
    }

    #[test]
    fn test_provider_field_null() -> TestResult {
        let json = r#"{"agents": [{"command": "claude", "provider": null}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents[0].provider, Some(ProviderConfig::None));
        assert_eq!(settings.agents[0].resolve_provider(), None);
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_provider_field_string() -> TestResult {
        let json = r#"{"agents": [{"command": "opencode", "provider": "copilot"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.agents[0].provider,
            Some(ProviderConfig::Explicit("copilot".to_string()))
        );
        assert_eq!(settings.agents[0].resolve_provider(), Some("copilot"));
        assert_eq!(settings.agents[0].resolve_domain(), Some("github.com"));
        Ok(())
    }

    #[test]
    fn test_priority_defaults_to_empty() {
        let settings = Settings::default();

        assert!(settings.priority.is_empty());
    }

    #[test]
    fn test_priority_defaults_to_zero_when_no_rule_matches() -> TestResult {
        let json = r#"{"priority": [{"command": "claude", "model": "high", "priority": 10}], "agents": [{"command": "codex"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.priority_for(&settings.agents[0], Some("high")), 0);
        assert_eq!(
            settings.priority_for_components("claude", Some("claude"), None),
            0
        );
        Ok(())
    }

    #[test]
    fn test_priority_matches_inferred_provider_and_model() -> TestResult {
        let json = r#"{
            "priority": [
                {"command": "claude", "model": "high", "priority": 42}
            ],
            "agents": [{"command": "claude"}]
        }"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.priority_for(&settings.agents[0], Some("high")), 42);
        Ok(())
    }

    #[test]
    fn test_priority_matches_null_provider_for_fallback_agent() -> TestResult {
        let json = r#"{
            "priority": [
                {"command": "claude", "provider": null, "model": "medium", "priority": 25}
            ],
            "agents": [{"command": "claude", "provider": null}]
        }"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.priority_for(&settings.agents[0], Some("medium")),
            25
        );
        Ok(())
    }

    #[test]
    fn test_priority_supports_full_i32_range() -> TestResult {
        let json = r#"{
            "priority": [
                {"command": "claude", "model": "high", "priority": 2147483647},
                {"command": "claude", "provider": null, "priority": -2147483648}
            ],
            "agents": [
                {"command": "claude"},
                {"command": "claude", "provider": null}
            ]
        }"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.priority_for(&settings.agents[0], Some("high")),
            i32::MAX
        );
        assert_eq!(settings.priority_for(&settings.agents[1], None), i32::MIN);
        Ok(())
    }

    #[test]
    fn test_command_codex_resolves_chatgpt_domain() -> TestResult {
        let json = r#"{"agents": [{"command": "codex"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert!(settings.agents[0].provider.is_none());
        assert_eq!(settings.agents[0].resolve_domain(), Some("chatgpt.com"));
        Ok(())
    }

    #[test]
    fn test_provider_field_codex_string() -> TestResult {
        let json = r#"{"agents": [{"command": "opencode", "provider": "codex"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.agents[0].provider,
            Some(ProviderConfig::Explicit("codex".to_string()))
        );
        assert_eq!(settings.agents[0].resolve_domain(), Some("chatgpt.com"));
        Ok(())
    }

    #[test]
    fn test_provider_unknown_string() -> TestResult {
        let json = r#"{"agents": [{"command": "someai", "provider": "unknown"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.agents[0].provider,
            Some(ProviderConfig::Explicit("unknown".to_string()))
        );
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_parse_minimal_settings_without_models() -> TestResult {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents.len(), 1);
        assert_eq!(settings.agents[0].command, "claude");
        assert!(settings.agents[0].args.is_empty());
        assert!(settings.agents[0].models.is_none());
        assert!(settings.agents[0].arg_maps.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_settings_with_env() -> TestResult {
        let json = r#"{"agents": [{"command": "claude", "env": {"ANTHROPIC_API_KEY": "sk-test", "CLAUDE_CODE_MAX_TURNS": "100"}}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        let env = settings.agents[0]
            .env
            .as_ref()
            .ok_or("env should be present")?;
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(env.get("CLAUDE_CODE_MAX_HOURS").map(String::as_str), None);
        assert_eq!(
            env.get("CLAUDE_CODE_MAX_TURNS").map(String::as_str),
            Some("100")
        );
        Ok(())
    }

    #[test]
    fn test_parse_settings_with_args_no_models() -> TestResult {
        let json = r#"{"agents": [{"command": "claude", "args": ["--permission-mode", "bypassPermissions"]}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.agents[0].args,
            ["--permission-mode", "bypassPermissions"]
        );
        assert!(settings.agents[0].models.is_none());
        assert!(settings.agents[0].arg_maps.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_jsonc_with_comments() -> TestResult {
        let jsonc = r#"{
            // This is a comment
            "agents": [
                {
                    "command": "claude", /* inline comment */
                    "args": ["--model", "{model}"]
                }
            ]
        }"#;
        let stripped = json_comments::StripComments::new(jsonc.as_bytes());
        let settings: Settings = serde_json::from_reader(stripped)?;
        assert_eq!(settings.agents.len(), 1);
        assert_eq!(settings.agents[0].command, "claude");
        Ok(())
    }

    #[test]
    fn test_parse_jsonc_with_trailing_commas() -> TestResult {
        let jsonc = r#"{
            // trailing commas
            "agents": [
                {
                    "command": "claude",
                    "args": ["--model", "{model}"],
                },
            ]
        }"#;
        let mut stripped = json_comments::StripComments::new(jsonc.as_bytes());
        let mut json_str = String::new();
        std::io::Read::read_to_string(&mut stripped, &mut json_str)?;
        let clean = strip_trailing_commas(&json_str);
        let settings: Settings = serde_json::from_str(&clean)?;
        assert_eq!(settings.agents.len(), 1);
        assert_eq!(settings.agents[0].command, "claude");
        Ok(())
    }

    #[test]
    fn test_parse_settings_with_arg_maps() -> TestResult {
        let json = r#"{"agents": [{"command": "claude", "arg_maps": {"--danger": ["--permission-mode", "bypassPermissions"]}}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.agents[0].arg_maps.get("--danger").cloned(),
            Some(vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
            ])
        );
        Ok(())
    }

    #[test]
    fn test_parse_settings_with_openrouter_management_key() -> TestResult {
        // Given: agent config with openrouter provider and management key
        let json = r#"{"agents": [{"command": "myai", "provider": "openrouter", "openrouter_management_key": "sk-or-v1-abc123"}]}"#;

        // When: parsed
        let settings: Settings = serde_json::from_str(json)?;

        // Then: key is correctly deserialized
        assert_eq!(
            settings.agents[0].openrouter_management_key.as_deref(),
            Some("sk-or-v1-abc123")
        );
        Ok(())
    }

    #[test]
    fn test_openrouter_management_key_defaults_to_none_when_absent() -> TestResult {
        // Given: agent config without openrouter_management_key field
        let json = r#"{"agents": [{"command": "claude"}]}"#;

        // When: parsed
        let settings: Settings = serde_json::from_str(json)?;

        // Then: key defaults to None
        assert!(settings.agents[0].openrouter_management_key.is_none());
        Ok(())
    }

    #[test]
    fn test_openrouter_provider_resolves_provider_but_not_domain() -> TestResult {
        // Given: agent with explicit "openrouter" provider (no cookie-based auth)
        let json = r#"{"agents": [{"command": "myai", "provider": "openrouter", "openrouter_management_key": "sk-or-v1-abc123"}]}"#;

        // When: provider and domain resolved
        let settings: Settings = serde_json::from_str(json)?;

        // Then: provider resolves to "openrouter" but domain is None
        // (OpenRouter does not use browser cookies)
        assert_eq!(settings.agents[0].resolve_provider(), Some("openrouter"));
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_parse_settings_with_pre_command() -> TestResult {
        let json =
            r#"{"agents": [{"command": "claude", "pre_command": ["git", "pull", "--rebase"]}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents[0].pre_command, ["git", "pull", "--rebase"]);
        Ok(())
    }

    #[test]
    fn test_pre_command_defaults_to_empty_when_absent() -> TestResult {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert!(settings.agents[0].pre_command.is_empty());
        Ok(())
    }

    #[test]
    fn test_openrouter_management_key_is_ignored_for_other_providers() -> TestResult {
        // Given: claude agent config that happens to have openrouter_management_key set
        let json = r#"{"agents": [{"command": "claude", "openrouter_management_key": "sk-or-v1-abc123"}]}"#;

        // When: parsed
        let settings: Settings = serde_json::from_str(json)?;

        // Then: provider resolution is unaffected by the presence of openrouter_management_key
        assert_eq!(settings.agents[0].resolve_provider(), Some("claude"));
        assert_eq!(settings.agents[0].resolve_domain(), Some("claude.ai"));
        Ok(())
    }

    // ── Serialize tests ──────────────────────────────────────────────────────

    #[test]
    fn test_serialize_roundtrip_sample_settings() -> TestResult {
        let settings = load_sample()?;
        let json = serde_json::to_string_pretty(&settings)?;
        let reparsed: Settings = serde_json::from_str(&json)?;

        assert_eq!(reparsed.agents.len(), settings.agents.len());
        assert_eq!(reparsed.priority.len(), settings.priority.len());
        assert_eq!(reparsed.agents[0].command, settings.agents[0].command);
        Ok(())
    }

    #[test]
    fn test_serialize_skips_empty_args() -> TestResult {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;
        let out = serde_json::to_string(&settings)?;
        let val: serde_json::Value = serde_json::from_str(&out)?;

        assert!(val["agents"][0]["args"].is_null());
        Ok(())
    }

    #[test]
    fn test_serialize_null_provider_roundtrip() -> TestResult {
        let json = r#"{"agents": [{"command": "claude", "provider": null}]}"#;
        let settings: Settings = serde_json::from_str(json)?;
        let out = serde_json::to_string(&settings)?;
        let val: serde_json::Value = serde_json::from_str(&out)?;

        assert!(val["agents"][0]["provider"].is_null());
        Ok(())
    }

    #[test]
    fn test_serialize_inferred_provider_skipped() -> TestResult {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;
        let out = serde_json::to_string(&settings)?;
        let val: serde_json::Value = serde_json::from_str(&out)?;

        // provider field absent when inferred
        assert!(val["agents"][0]["provider"].is_null());
        Ok(())
    }

    #[test]
    fn test_upsert_priority_creates_new_rule() {
        let mut settings = Settings::default();
        settings.upsert_priority("claude", None, Some("high".to_string()), 42);

        assert_eq!(settings.priority.len(), 1);
        assert_eq!(settings.priority[0].priority, 42);
        assert_eq!(settings.priority[0].model.as_deref(), Some("high"));
    }

    #[test]
    fn test_upsert_priority_updates_existing_rule() {
        let mut settings = Settings::default();
        settings.upsert_priority("claude", None, Some("high".to_string()), 10);
        settings.upsert_priority("claude", None, Some("high".to_string()), 99);

        assert_eq!(settings.priority.len(), 1);
        assert_eq!(settings.priority[0].priority, 99);
    }

    #[test]
    fn test_remove_priority_removes_matching_rule() {
        let mut settings = Settings::default();
        settings.upsert_priority("claude", None, Some("high".to_string()), 10);
        settings.upsert_priority("claude", None, Some("low".to_string()), 5);
        settings.remove_priority("claude", None, Some("high"));

        assert_eq!(settings.priority.len(), 1);
        assert_eq!(settings.priority[0].model.as_deref(), Some("low"));
    }

    #[test]
    fn test_save_and_reload() -> TestResult {
        let settings = load_sample()?;
        let tmp = tempfile::NamedTempFile::new()?;
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        let reloaded: Settings = serde_json::from_str(&content)?;

        assert_eq!(reloaded.agents.len(), settings.agents.len());
        assert_eq!(reloaded.priority.len(), settings.priority.len());
        Ok(())
    }

    #[test]
    fn test_save_preserves_comments() -> TestResult {
        let jsonc = r#"{
    // This is a top-level comment
    "agents": [
        {"command": "claude"}
    ]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let settings = Settings::load(Some(tmp.path()))?;
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(content.contains("// This is a top-level comment"));
        assert!(content.contains("claude"));
        Ok(())
    }

    #[test]
    fn test_save_plain_json_roundtrip_via_load() -> TestResult {
        let json = r#"{"agents": [{"command": "claude"}]}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        let settings = Settings::load(Some(tmp.path()))?;
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        let reloaded = Settings::load(Some(tmp.path()))?;
        assert_eq!(reloaded.agents.len(), 1);
        assert_eq!(reloaded.agents[0].command, "claude");
        // Should be valid JSON (parseable)
        let _: serde_json::Value = serde_json::from_str(&content)?;
        Ok(())
    }

    #[test]
    fn test_save_with_added_agent_preserves_comments() -> TestResult {
        let jsonc = r#"{
    // Top comment
    "agents": [
        {"command": "claude"}
    ]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let mut settings = Settings::load(Some(tmp.path()))?;
        settings.agents.push(AgentConfig {
            command: "codex".to_string(),
            args: vec![],
            models: None,
            arg_maps: HashMap::new(),
            env: None,
            provider: None,
            openrouter_management_key: None,
            pre_command: vec![],
        });
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(content.contains("// Top comment"));
        assert!(content.contains("codex"));
        Ok(())
    }

    #[test]
    fn test_serde_value_to_cst_input_variants() {
        use jsonc_parser::cst::CstInputValue;

        assert!(matches!(
            serde_value_to_cst_input(&serde_json::Value::Null),
            CstInputValue::Null
        ));
        assert!(matches!(
            serde_value_to_cst_input(&serde_json::Value::Bool(true)),
            CstInputValue::Bool(true)
        ));
        assert!(matches!(
            serde_value_to_cst_input(&serde_json::Value::String("hi".to_string())),
            CstInputValue::String(s) if s == "hi"
        ));
        assert!(matches!(
            serde_value_to_cst_input(&serde_json::json!(42)),
            CstInputValue::Number(n) if n == "42"
        ));
        assert!(matches!(
            serde_value_to_cst_input(&serde_json::json!([])),
            CstInputValue::Array(v) if v.is_empty()
        ));
        assert!(matches!(
            serde_value_to_cst_input(&serde_json::json!({})),
            CstInputValue::Object(v) if v.is_empty()
        ));
    }
}
