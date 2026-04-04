use chrono::{DateTime, Local};
use jsonc_parser::cst::{
    CstArray, CstContainerNode, CstInputValue, CstLeafNode, CstNode, CstObject, CstRootNode,
};
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
/// - `Inferred`: field absent -> provider is inferred from the command name
/// - `Explicit(name)`: field has a string value -> use that provider name
/// - `None`: field is `null` -> no provider (fallback agent)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glm_api_key: Option<String>,
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
    /// Weekday ranges in "start-end" format (0=Sun, 1=Mon, ..., 6=Sat, inclusive).
    /// e.g. `["1-5"]` means Monday through Friday.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<String>>,
    /// Hour ranges in "start-end" format, half-open [start, end), 0-48.
    /// e.g. `["21-27"]` means 21:00 to 03:00 next day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<Vec<String>>,
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

    #[must_use]
    pub fn has_model(&self, model_key: &str) -> bool {
        self.models
            .as_ref()
            .is_none_or(|m| m.contains_key(model_key))
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

    /// Like [`matches`], but also evaluates `weekdays`/`hours` schedule conditions
    /// against the given local timestamp.
    #[must_use]
    pub fn matches_at(
        &self,
        command: &str,
        provider: Option<&str>,
        model: Option<&str>,
        now: &DateTime<Local>,
    ) -> bool {
        use chrono::{Datelike, Timelike};

        if !self.matches(command, provider, model) {
            return false;
        }

        let current_hour = now.hour();
        let current_weekday = now.weekday().num_days_from_sunday(); // 0=Sun..6=Sat

        // Check hours constraint (OR logic across multiple ranges)
        if let Some(hour_ranges) = &self.hours {
            let hour_matched = hour_ranges.iter().any(|range_str| {
                let Some((start, end)) = parse_schedule_range(range_str) else {
                    return false;
                };
                // Direct match: current_hour in [start, end)
                if current_hour >= start && current_hour < end {
                    weekday_in_ranges(current_weekday, self.weekdays.as_deref())
                }
                // Overnight match: (current_hour + 24) in [start, end)
                else if end > 24 {
                    let shifted = current_hour + 24;
                    if shifted >= start && shifted < end {
                        // Use previous day's weekday
                        let prev_weekday = if current_weekday == 0 {
                            6
                        } else {
                            current_weekday - 1
                        };
                        weekday_in_ranges(prev_weekday, self.weekdays.as_deref())
                    } else {
                        false
                    }
                } else {
                    false
                }
            });
            if !hour_matched {
                return false;
            }
        } else if !weekday_in_ranges(current_weekday, self.weekdays.as_deref()) {
            // No hours constraint; check weekdays alone if present
            return false;
        }

        true
    }

    /// Returns the number of schedule axes constrained by this rule (0, 1, or 2).
    /// Used for conflict resolution: rules with more constraints win over less specific ones.
    #[must_use]
    pub fn schedule_specificity(&self) -> u8 {
        u8::from(self.weekdays.is_some()) + u8::from(self.hours.is_some())
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
                glm_api_key: None,
                pre_command: vec![],
            }],
            original_text: None,
        }
    }
}

fn merge_cst_node(cst_node: &CstNode, new_val: &serde_json::Value) {
    match new_val {
        serde_json::Value::Object(obj) => {
            if let Some(cst_obj) = cst_node.as_object() {
                merge_cst_object(&cst_obj, obj);
                return;
            }
        }
        serde_json::Value::Array(arr) => {
            if let Some(cst_arr) = cst_node.as_array() {
                merge_cst_array(&cst_arr, arr);
                return;
            }
        }
        serde_json::Value::String(s) => {
            if let Some(lit) = cst_node.as_string_lit() {
                if lit.decoded_value().ok().as_deref() != Some(s.as_str())
                    && let Ok(raw) = serde_json::to_string(s)
                {
                    lit.set_raw_value(raw);
                }
                return;
            }
        }
        serde_json::Value::Number(n) => {
            if let Some(lit) = cst_node.as_number_lit() {
                let new_text = n.to_string();
                if lit.to_string() != new_text {
                    lit.set_raw_value(new_text);
                }
                return;
            }
        }
        serde_json::Value::Bool(b) => {
            if let Some(lit) = cst_node.as_boolean_lit() {
                if lit.value() != *b {
                    lit.set_value(*b);
                }
                return;
            }
        }
        serde_json::Value::Null => {
            if cst_node.as_null_keyword().is_some() {
                return;
            }
        }
    }
    // Type mismatch: replace the node entirely
    let replacement = serde_value_to_cst_input(new_val);
    if let Some(prop) = cst_node.parent().and_then(|p| p.as_object_prop()) {
        prop.set_value(replacement);
    } else {
        // Array element or other context: use type-specific replace_with
        replace_cst_node(cst_node.clone(), replacement);
    }
}

fn replace_cst_node(node: CstNode, replacement: CstInputValue) {
    match node {
        CstNode::Leaf(leaf) => match leaf {
            CstLeafNode::StringLit(n) => {
                n.replace_with(replacement);
            }
            CstLeafNode::NumberLit(n) => {
                n.replace_with(replacement);
            }
            CstLeafNode::BooleanLit(n) => {
                n.replace_with(replacement);
            }
            CstLeafNode::NullKeyword(n) => {
                n.replace_with(replacement);
            }
            CstLeafNode::WordLit(_)
            | CstLeafNode::Token(_)
            | CstLeafNode::Whitespace(_)
            | CstLeafNode::Newline(_)
            | CstLeafNode::Comment(_) => {}
        },
        CstNode::Container(container) => match container {
            CstContainerNode::Object(n) => {
                n.replace_with(replacement);
            }
            CstContainerNode::Array(n) => {
                n.replace_with(replacement);
            }
            CstContainerNode::Root(_) | CstContainerNode::ObjectProp(_) => {}
        },
    }
}

fn merge_cst_object(cst_obj: &CstObject, new_obj: &serde_json::Map<String, serde_json::Value>) {
    for (key, val) in new_obj {
        if let Some(prop) = cst_obj.get(key) {
            if let Some(existing) = prop.value() {
                merge_cst_node(&existing, val);
            } else {
                prop.set_value(serde_value_to_cst_input(val));
            }
        } else {
            cst_obj.append(key, serde_value_to_cst_input(val));
        }
    }
    let props_to_remove: Vec<_> = cst_obj
        .properties()
        .into_iter()
        .filter(|prop| {
            prop.name()
                .and_then(|n| n.decoded_value().ok())
                .is_some_and(|name| !new_obj.contains_key(&name))
        })
        .collect();
    for prop in props_to_remove {
        prop.remove();
    }
}

fn merge_cst_array(cst_arr: &CstArray, new_arr: &[serde_json::Value]) {
    let elements = cst_arr.elements();
    let existing_len = elements.len();
    let new_len = new_arr.len();

    // Update existing elements in place
    for (i, new_val) in new_arr.iter().enumerate().take(existing_len) {
        merge_cst_node(&elements[i], new_val);
    }

    // Remove extra elements from the end (in reverse to keep indices stable)
    for element in elements.into_iter().skip(new_len).rev() {
        element.remove();
    }

    // Append new elements
    for new_val in new_arr.iter().skip(existing_len) {
        cst_arr.append(serde_value_to_cst_input(new_val));
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

/// Parse a schedule range string like "start-end" into `(start, end)`.
/// Returns `None` if the string is not parseable.
/// Validation (start < end, bounds) is done separately in `validate_priority_schedule`.
fn parse_schedule_range(s: &str) -> Option<(u32, u32)> {
    let (start_str, end_str) = s.split_once('-')?;
    let start: u32 = start_str.parse().ok()?;
    let end: u32 = end_str.parse().ok()?;
    Some((start, end))
}

/// Returns `true` if `weekday` falls within any range in `ranges`, or if `ranges` is `None`.
fn weekday_in_ranges(weekday: u32, ranges: Option<&[String]>) -> bool {
    if let Some(wd_ranges) = ranges {
        wd_ranges.iter().any(|wd_str| {
            let Some((ws, we)) = parse_schedule_range(wd_str) else {
                return false;
            };
            weekday >= ws && weekday <= we
        })
    } else {
        true
    }
}

impl Settings {
    /// Evaluates schedule conditions against `now`.
    /// Among all matching rules, the one with the highest schedule specificity wins;
    /// ties are broken by first occurrence (stable, original-order compatible).
    #[must_use]
    pub fn priority_for_at(
        &self,
        agent: &AgentConfig,
        model: Option<&str>,
        now: &DateTime<Local>,
    ) -> i32 {
        self.priority_for_components_at(&agent.command, agent.resolve_provider(), model, now)
    }

    /// Among all matching rules, the one with the highest schedule specificity wins;
    /// ties are broken by first occurrence (stable, original-order compatible).
    #[must_use]
    pub fn priority_for_components_at(
        &self,
        command: &str,
        provider: Option<&str>,
        model: Option<&str>,
        now: &DateTime<Local>,
    ) -> i32 {
        // Among all matching rules, pick the one with the highest schedule_specificity.
        // In case of a tie, the first occurrence wins (stable, original-order compatible).
        self.priority
            .iter()
            .filter(|rule| rule.matches_at(command, provider, model, now))
            .fold(None::<&PriorityRule>, |best, rule| match best {
                None => Some(rule),
                Some(b) if rule.schedule_specificity() > b.schedule_specificity() => Some(rule),
                Some(b) => Some(b),
            })
            .map_or(0, |rule| rule.priority)
    }

    fn validate_priority_schedule(&self) -> Result<(), Box<dyn std::error::Error>> {
        for rule in &self.priority {
            if let Some(hour_ranges) = &rule.hours {
                for range_str in hour_ranges {
                    let (start, end) = parse_schedule_range(range_str)
                        .ok_or_else(|| format!("invalid hours range: {range_str:?}"))?;
                    if start >= end {
                        return Err(format!(
                            "invalid hours range {range_str:?}: start must be less than end"
                        )
                        .into());
                    }
                    if end > 48 {
                        return Err(format!(
                            "invalid hours range {range_str:?}: end must not exceed 48"
                        )
                        .into());
                    }
                }
            }
            if let Some(wd_ranges) = &rule.weekdays {
                for range_str in wd_ranges {
                    let (start, end) = parse_schedule_range(range_str)
                        .ok_or_else(|| format!("invalid weekdays range: {range_str:?}"))?;
                    if start > end {
                        return Err(format!(
                            "invalid weekdays range {range_str:?}: start must not exceed end"
                        )
                        .into());
                    }
                    if end > 6 {
                        return Err(format!(
                            "invalid weekdays range {range_str:?}: end must not exceed 6"
                        )
                        .into());
                    }
                }
            }
        }
        Ok(())
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
        settings.validate_priority_schedule()?;
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

        merge_cst_object(&root_obj, obj);

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
            weekdays: None,
            hours: None,
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

/// Construct a local `DateTime` for testing without DST ambiguity (January = no DST).
#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test helper")]
pub(crate) fn make_local_dt(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Local> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::new(
        chrono::NaiveDate::from_ymd_opt(year, month, day).unwrap(),
        chrono::NaiveTime::from_hms_opt(hour, 0, 0).unwrap(),
    );
    Local
        .from_local_datetime(&naive)
        .single()
        .unwrap_or_else(|| Local.from_local_datetime(&naive).latest().unwrap())
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
                weekdays: None,
                hours: None,
            }
        );
        assert_eq!(
            settings.priority[2],
            PriorityRule {
                command: "claude".to_string(),
                provider: Some(ProviderConfig::None),
                model: Some("medium".to_string()),
                priority: 25,
                weekdays: None,
                hours: None,
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

        // no provider field -> None (inferred from command name)
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

        // provider: "copilot" -> Some(Explicit("copilot"))
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

        // provider: null -> Some(ProviderConfig::None) (fallback)
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
        let now = make_local_dt(2024, 1, 8, 10);

        assert_eq!(
            settings.priority_for_at(&settings.agents[0], Some("high"), &now),
            0
        );
        assert_eq!(
            settings.priority_for_components_at("claude", Some("claude"), None, &now),
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
        let now = make_local_dt(2024, 1, 8, 10);

        assert_eq!(
            settings.priority_for_at(&settings.agents[0], Some("high"), &now),
            42
        );
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
        let now = make_local_dt(2024, 1, 8, 10);

        assert_eq!(
            settings.priority_for_at(&settings.agents[0], Some("medium"), &now),
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
        let now = make_local_dt(2024, 1, 8, 10);

        assert_eq!(
            settings.priority_for_at(&settings.agents[0], Some("high"), &now),
            i32::MAX
        );
        assert_eq!(
            settings.priority_for_at(&settings.agents[1], None, &now),
            i32::MIN
        );
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
    fn test_provider_field_opencode_go_string() -> TestResult {
        let json = r#"{"agents": [{"command": "opencode", "provider": "opencode-go"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(
            settings.agents[0].provider,
            Some(ProviderConfig::Explicit("opencode-go".to_string()))
        );
        assert_eq!(settings.agents[0].resolve_provider(), Some("opencode-go"));
        assert_eq!(settings.agents[0].resolve_domain(), None);
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

    // -- Serialize tests ------------------------------------------------------

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
            glm_api_key: None,
            pre_command: vec![],
        });
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(content.contains("// Top comment"));
        assert!(content.contains("codex"));
        Ok(())
    }

    #[test]
    fn test_save_preserves_inline_comments_inside_agents_array() -> TestResult {
        let jsonc = r#"{
    "agents": [
        // Claude agent configuration
        {
            "command": "claude",
            "args": ["--model", "{model}"],
            // Model name mapping
            "models": {
                "high": "opus",
                "medium": "sonnet"
            }
        },
        // Codex agent
        {"command": "codex"}
    ]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let settings = Settings::load(Some(tmp.path()))?;
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(
            content.contains("// Claude agent configuration"),
            "comment before first agent lost:\n{content}"
        );
        assert!(
            content.contains("// Model name mapping"),
            "comment inside agent object lost:\n{content}"
        );
        assert!(
            content.contains("// Codex agent"),
            "comment before second agent lost:\n{content}"
        );
        Ok(())
    }

    #[test]
    fn test_save_preserves_comments_after_modifying_agent() -> TestResult {
        let jsonc = r#"{
    "agents": [
        // My main agent
        {
            "command": "claude",
            // important args
            "args": ["--model", "{model}"]
        }
    ]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let mut settings = Settings::load(Some(tmp.path()))?;
        settings.agents[0].command = "opencode".to_string();
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(
            content.contains("// My main agent"),
            "comment before agent lost:\n{content}"
        );
        assert!(
            content.contains("// important args"),
            "comment inside agent lost:\n{content}"
        );
        assert!(
            content.contains("opencode"),
            "command not updated:\n{content}"
        );
        Ok(())
    }

    #[test]
    fn test_save_preserves_comments_when_removing_agent() -> TestResult {
        let jsonc = r#"{
    // top-level comment
    "agents": [
        // first agent
        {"command": "claude"},
        // second agent
        {"command": "codex"}
    ]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let mut settings = Settings::load(Some(tmp.path()))?;
        settings.agents.remove(1); // remove codex
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(
            content.contains("// top-level comment"),
            "top-level comment lost:\n{content}"
        );
        assert!(
            content.contains("// first agent"),
            "first agent comment lost:\n{content}"
        );
        assert!(
            content.contains("claude"),
            "claude not preserved:\n{content}"
        );
        // codex should be gone
        assert!(
            !content.contains("codex"),
            "codex should be removed:\n{content}"
        );
        Ok(())
    }

    #[test]
    fn test_save_preserves_comments_with_priority_and_agent_change() -> TestResult {
        let jsonc = r#"{
    // Global priority rules
    "priority": [
        {"command": "claude", "model": "high", "priority": 50}
    ],
    // Agent configurations
    "agents": [
        // Claude Code agent - primary
        {
            "command": "claude",
            "args": ["--model", "{model}"],
            // Model name mapping
            "models": {
                "high": "opus",
                "medium": "sonnet"
            }
        },
        // Codex agent - secondary
        {"command": "codex"}
    ]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let mut settings = Settings::load(Some(tmp.path()))?;
        // Simulate GUI: change agent command and add a priority rule
        settings.agents[0].command = "opencode".to_string();
        settings.upsert_priority("opencode", None, Some("high".to_string()), 50);
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;
        assert!(
            content.contains("// Global priority rules"),
            "top-level priority comment lost:\n{content}"
        );
        assert!(
            content.contains("// Agent configurations"),
            "top-level agents comment lost:\n{content}"
        );
        assert!(
            content.contains("// Claude Code agent - primary"),
            "comment before first agent lost:\n{content}"
        );
        assert!(
            content.contains("// Model name mapping"),
            "comment inside agent object lost:\n{content}"
        );
        assert!(
            content.contains("// Codex agent - secondary"),
            "comment before second agent lost:\n{content}"
        );
        assert!(
            content.contains("opencode"),
            "command not updated:\n{content}"
        );
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

    #[test]
    fn test_command_zai_infers_provider_zai() -> TestResult {
        let json = r#"{"agents": [{"command": "zai"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents[0].resolve_provider(), Some("zai"));
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_command_kimik2_infers_provider_kimik2() -> TestResult {
        let json = r#"{"agents": [{"command": "kimi-k2"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents[0].resolve_provider(), Some("kimi-k2"));
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_command_warp_infers_provider_warp() -> TestResult {
        let json = r#"{"agents": [{"command": "warp"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents[0].resolve_provider(), Some("warp"));
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    #[test]
    fn test_command_kiro_infers_provider_kiro() -> TestResult {
        let json = r#"{"agents": [{"command": "kiro"}]}"#;
        let settings: Settings = serde_json::from_str(json)?;

        assert_eq!(settings.agents[0].resolve_provider(), Some("kiro"));
        assert_eq!(settings.agents[0].resolve_domain(), None);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // helpers for schedule tests
    // -----------------------------------------------------------------------

    fn make_scheduled_rule(
        command: &str,
        priority: i32,
        weekdays: Option<Vec<&str>>,
        hours: Option<Vec<&str>>,
    ) -> PriorityRule {
        PriorityRule {
            command: command.to_string(),
            provider: None,
            model: None,
            priority,
            weekdays: weekdays.map(|v| {
                v.into_iter()
                    .map(std::string::ToString::to_string)
                    .collect()
            }),
            hours: hours.map(|v| {
                v.into_iter()
                    .map(std::string::ToString::to_string)
                    .collect()
            }),
        }
    }

    // -----------------------------------------------------------------------
    // PriorityRule schedule fields: serde
    // -----------------------------------------------------------------------

    #[test]
    fn test_priority_rule_deserializes_weekdays_and_hours() -> TestResult {
        // Given: a priority rule JSON with weekdays and hours
        let json = r#"{
            "priority": [{
                "command": "codex",
                "priority": 200,
                "weekdays": ["1-5"],
                "hours": ["21-27"]
            }],
            "agents": []
        }"#;

        // When: parsed
        let settings: Settings = serde_json::from_str(json)?;

        // Then: fields are populated correctly
        assert_eq!(settings.priority[0].weekdays, Some(vec!["1-5".to_string()]));
        assert_eq!(settings.priority[0].hours, Some(vec!["21-27".to_string()]));
        Ok(())
    }

    #[test]
    fn test_priority_rule_weekdays_hours_absent_defaults_to_none() -> TestResult {
        // Given: a priority rule without weekdays/hours
        let json = r#"{"priority": [{"command": "codex", "priority": 50}], "agents": []}"#;

        // When: parsed
        let settings: Settings = serde_json::from_str(json)?;

        // Then: optional schedule fields are None
        assert_eq!(settings.priority[0].weekdays, None);
        assert_eq!(settings.priority[0].hours, None);
        Ok(())
    }

    #[test]
    fn test_priority_rule_serializes_without_schedule_fields_when_none() -> TestResult {
        // Given: a rule with no schedule fields
        let rule = make_scheduled_rule("codex", 50, None, None);
        let settings = Settings {
            priority: vec![rule],
            agents: vec![],
            original_text: None,
        };

        // When: serialized to JSON
        let json = serde_json::to_string(&settings)?;
        let val: serde_json::Value = serde_json::from_str(&json)?;

        // Then: weekdays and hours fields are absent
        assert!(
            val["priority"][0]["weekdays"].is_null(),
            "weekdays should be absent when None"
        );
        assert!(
            val["priority"][0]["hours"].is_null(),
            "hours should be absent when None"
        );
        Ok(())
    }

    #[test]
    fn test_priority_rule_serializes_with_schedule_fields_when_some() -> TestResult {
        // Given: a rule with weekdays and hours
        let rule = make_scheduled_rule("codex", 200, Some(vec!["1-5"]), Some(vec!["21-27"]));
        let settings = Settings {
            priority: vec![rule],
            agents: vec![],
            original_text: None,
        };

        // When: serialized to JSON
        let json = serde_json::to_string(&settings)?;
        let val: serde_json::Value = serde_json::from_str(&json)?;

        // Then: weekdays and hours are present with correct values
        assert_eq!(val["priority"][0]["weekdays"], serde_json::json!(["1-5"]));
        assert_eq!(val["priority"][0]["hours"], serde_json::json!(["21-27"]));
        Ok(())
    }

    #[test]
    fn test_priority_rule_multiple_hour_ranges_serialize_and_deserialize() -> TestResult {
        // Given: multiple hour ranges
        let json = r#"{
            "priority": [{"command": "codex", "priority": 100, "hours": ["1-7", "21-27"]}],
            "agents": []
        }"#;

        // When: parsed
        let settings: Settings = serde_json::from_str(json)?;

        // Then: both ranges are preserved
        assert_eq!(
            settings.priority[0].hours,
            Some(vec!["1-7".to_string(), "21-27".to_string()])
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Range validation on load
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_rejects_hours_range_where_start_equals_end() -> TestResult {
        // Given: hours range "7-7" where start == end (invalid: not start < end)
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "hours": ["7-7"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load returns an error
        let result = Settings::load(Some(tmp.path()));
        assert!(
            result.is_err(),
            "expected load error for hours range where start == end"
        );
        Ok(())
    }

    #[test]
    fn test_load_rejects_hours_range_where_start_exceeds_end() -> TestResult {
        // Given: hours range "10-5" where start > end (invalid)
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "hours": ["10-5"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load returns an error
        let result = Settings::load(Some(tmp.path()));
        assert!(
            result.is_err(),
            "expected load error for hours range where start > end"
        );
        Ok(())
    }

    #[test]
    fn test_load_rejects_hours_end_exceeds_48() -> TestResult {
        // Given: hours end value 49 which exceeds the maximum of 48
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "hours": ["20-49"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load returns an error
        let result = Settings::load(Some(tmp.path()));
        assert!(result.is_err(), "expected load error for hours end > 48");
        Ok(())
    }

    #[test]
    fn test_load_rejects_weekday_range_where_start_exceeds_end() -> TestResult {
        // Given: weekdays range "5-2" where start > end
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "weekdays": ["5-2"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load returns an error
        let result = Settings::load(Some(tmp.path()));
        assert!(
            result.is_err(),
            "expected load error for weekdays range where start > end"
        );
        Ok(())
    }

    #[test]
    fn test_load_rejects_weekday_value_exceeds_6() -> TestResult {
        // Given: weekdays range with end value 7 which is beyond Saturday (6)
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "weekdays": ["1-7"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load returns an error
        let result = Settings::load(Some(tmp.path()));
        assert!(result.is_err(), "expected load error for weekday value > 6");
        Ok(())
    }

    #[test]
    fn test_load_accepts_valid_hour_range_0_to_24() -> TestResult {
        // Given: valid hours range "0-24"
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "hours": ["0-24"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load succeeds
        let result = Settings::load(Some(tmp.path()));
        assert!(
            result.is_ok(),
            "expected load to succeed for valid hour range"
        );
        Ok(())
    }

    #[test]
    fn test_load_accepts_valid_weekday_range_1_to_5() -> TestResult {
        // Given: valid weekdays range "1-5" (Mon-Fri)
        let json = r#"{"priority": [{"command": "codex", "priority": 50, "weekdays": ["1-5"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load succeeds
        let result = Settings::load(Some(tmp.path()));
        assert!(
            result.is_ok(),
            "expected load to succeed for valid weekday range"
        );
        Ok(())
    }

    #[test]
    fn test_load_accepts_overnight_hour_range_21_to_27() -> TestResult {
        // Given: valid overnight hours range "21-27"
        let json = r#"{"priority": [{"command": "codex", "priority": 200, "hours": ["21-27"]}], "agents": []}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), json)?;

        // When/Then: load succeeds
        let result = Settings::load(Some(tmp.path()));
        assert!(
            result.is_ok(),
            "expected load to succeed for overnight hour range"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // PriorityRule::matches_at
    // -----------------------------------------------------------------------

    #[test]
    fn test_matches_at_no_schedule_always_matches_any_time() {
        // Given: base rule with no weekdays/hours
        let rule = make_scheduled_rule("codex", 50, None, None);
        let monday_22h = make_local_dt(2024, 1, 8, 22);
        let saturday_3h = make_local_dt(2024, 1, 13, 3);

        // When/Then: matches at any time
        assert!(rule.matches_at("codex", Some("codex"), None, &monday_22h));
        assert!(rule.matches_at("codex", Some("codex"), None, &saturday_3h));
    }

    #[test]
    fn test_matches_at_no_schedule_wrong_command_returns_false() {
        // Given: rule for "codex" with no schedule
        let rule = make_scheduled_rule("codex", 50, None, None);
        let monday_22h = make_local_dt(2024, 1, 8, 22);

        // When/Then: a different command does not match
        assert!(!rule.matches_at("claude", Some("claude"), None, &monday_22h));
    }

    #[test]
    fn test_matches_at_weekdays_matches_on_specified_weekday() {
        // Given: rule restricted to Mon-Fri (weekdays "1-5")
        let rule = make_scheduled_rule("codex", 200, Some(vec!["1-5"]), None);
        // 2024-01-08 is Monday (weekday 1)
        let monday = make_local_dt(2024, 1, 8, 10);

        // When/Then: Monday is within "1-5"
        assert!(rule.matches_at("codex", Some("codex"), None, &monday));
    }

    #[test]
    fn test_matches_at_weekdays_no_match_on_off_day() {
        // Given: rule restricted to Mon-Fri (weekdays "1-5")
        let rule = make_scheduled_rule("codex", 200, Some(vec!["1-5"]), None);
        // 2024-01-06 is Saturday (weekday 6)
        let saturday = make_local_dt(2024, 1, 6, 10);

        // When/Then: Saturday is NOT within "1-5"
        assert!(!rule.matches_at("codex", Some("codex"), None, &saturday));
    }

    #[test]
    fn test_matches_at_weekdays_sunday_included_in_0_to_0_range() {
        // Given: rule restricted to only Sunday (weekdays "0-0")
        let rule = make_scheduled_rule("codex", 200, Some(vec!["0-0"]), None);
        // 2024-01-07 is Sunday (weekday 0)
        let sunday = make_local_dt(2024, 1, 7, 10);
        let monday = make_local_dt(2024, 1, 8, 10);

        // When/Then
        assert!(rule.matches_at("codex", Some("codex"), None, &sunday));
        assert!(!rule.matches_at("codex", Some("codex"), None, &monday));
    }

    #[test]
    fn test_matches_at_hours_matches_within_range() {
        // Given: rule restricted to hours "21-27" (21:00-03:00)
        let rule = make_scheduled_rule("codex", 200, None, Some(vec!["21-27"]));
        let monday_22h = make_local_dt(2024, 1, 8, 22);

        // When/Then: 22:00 is within [21, 27)
        assert!(rule.matches_at("codex", Some("codex"), None, &monday_22h));
    }

    #[test]
    fn test_matches_at_hours_no_match_outside_range() {
        // Given: rule restricted to hours "21-27"
        let rule = make_scheduled_rule("codex", 200, None, Some(vec!["21-27"]));
        let monday_20h = make_local_dt(2024, 1, 8, 20);

        // When/Then: 20:00 is NOT in [21, 27)
        assert!(!rule.matches_at("codex", Some("codex"), None, &monday_20h));
    }

    #[test]
    fn test_matches_at_hours_half_open_end_boundary_does_not_match() {
        // Given: rule restricted to hours "21-24" (21:00 to midnight, half-open)
        let rule = make_scheduled_rule("codex", 200, None, Some(vec!["21-24"]));
        // At exactly hour 24 (= next day 00:00), direct match: 24 not in [21, 24)
        // Cross-midnight: 24+0=24, not in [21, 24) either
        let tuesday_0h = make_local_dt(2024, 1, 9, 0);

        // When/Then: midnight (hour 0) is NOT matched by direct or cross-midnight path
        // Cross-midnight: 24+0=24, half-open [21, 24) excludes 24
        assert!(!rule.matches_at("codex", Some("codex"), None, &tuesday_0h));
    }

    #[test]
    fn test_matches_at_hours_multiple_ranges_uses_or_logic() {
        // Given: rule with two separate hour ranges "1-7" and "21-27"
        let rule = make_scheduled_rule("codex", 200, None, Some(vec!["1-7", "21-27"]));
        let monday_3h = make_local_dt(2024, 1, 8, 3);
        let monday_22h = make_local_dt(2024, 1, 8, 22);
        let monday_12h = make_local_dt(2024, 1, 8, 12);

        // When/Then: both ranges match, middle of day does not
        assert!(rule.matches_at("codex", Some("codex"), None, &monday_3h));
        assert!(rule.matches_at("codex", Some("codex"), None, &monday_22h));
        assert!(!rule.matches_at("codex", Some("codex"), None, &monday_12h));
    }

    #[test]
    fn test_matches_at_overnight_range_matches_hours_in_next_day_morning() {
        // Given: rule with hours "21-27" (21:00 Mon to 03:00 Tue)
        let rule = make_scheduled_rule("codex", 200, None, Some(vec!["21-27"]));
        // 2024-01-09 02:00 is Tuesday 02:00
        // Cross-midnight check: previous day's hour = 24+2 = 26, which is in [21, 27)
        let tuesday_2h = make_local_dt(2024, 1, 9, 2);

        // When/Then: Tuesday 02:00 matches because 24+2=26 is in [21, 27)
        assert!(rule.matches_at("codex", Some("codex"), None, &tuesday_2h));
    }

    #[test]
    fn test_matches_at_overnight_range_does_not_match_at_boundary_end() {
        // Given: rule with hours "21-27"
        let rule = make_scheduled_rule("codex", 200, None, Some(vec!["21-27"]));
        // 2024-01-09 03:00: cross-midnight check gives 24+3=27, which is NOT in [21, 27)
        let tuesday_3h = make_local_dt(2024, 1, 9, 3);

        // When/Then: Tuesday 03:00 does NOT match (27 excluded by half-open interval)
        assert!(!rule.matches_at("codex", Some("codex"), None, &tuesday_3h));
    }

    #[test]
    fn test_matches_at_overnight_with_weekday_uses_start_day_for_cross_midnight_hour() {
        // Given: rule for Mon-Fri with hours "21-27"
        // Monday 21:00 to Tuesday 03:00 should be active
        let rule = make_scheduled_rule("codex", 200, Some(vec!["1-5"]), Some(vec!["21-27"]));
        // Tuesday 02:00: cross-midnight → previous day is Monday (weekday 1), which is in "1-5"
        let tuesday_2h = make_local_dt(2024, 1, 9, 2);

        // When/Then: Tuesday 02:00 matches because it falls within Monday's 21-27 window
        assert!(rule.matches_at("codex", Some("codex"), None, &tuesday_2h));
    }

    #[test]
    fn test_matches_at_overnight_with_weekday_no_match_when_start_day_excluded() {
        // Given: rule for Mon-Fri with hours "21-27"
        // Saturday 21:00 to Sunday 03:00 should NOT be active (Saturday is not in 1-5)
        let rule = make_scheduled_rule("codex", 200, Some(vec!["1-5"]), Some(vec!["21-27"]));
        // Sunday 02:00: cross-midnight → previous day is Saturday (weekday 6), not in "1-5"
        let sunday_2h = make_local_dt(2024, 1, 7, 2);

        // When/Then: does not match
        assert!(!rule.matches_at("codex", Some("codex"), None, &sunday_2h));
    }

    #[test]
    fn test_matches_at_weekdays_and_hours_both_required_for_match() {
        // Given: rule for Mon-Fri with hours "9-17"
        let rule = make_scheduled_rule("codex", 200, Some(vec!["1-5"]), Some(vec!["9-17"]));
        let monday_10h = make_local_dt(2024, 1, 8, 10); // Mon 10:00 → matches
        let monday_20h = make_local_dt(2024, 1, 8, 20); // Mon 20:00 → wrong hour
        let saturday_10h = make_local_dt(2024, 1, 13, 10); // Sat 10:00 → wrong day

        // When/Then
        assert!(rule.matches_at("codex", Some("codex"), None, &monday_10h));
        assert!(!rule.matches_at("codex", Some("codex"), None, &monday_20h));
        assert!(!rule.matches_at("codex", Some("codex"), None, &saturday_10h));
    }

    // -----------------------------------------------------------------------
    // Settings::priority_for_at
    // -----------------------------------------------------------------------

    #[test]
    fn test_priority_for_at_backward_compat_no_schedule_behaves_like_priority_for() -> TestResult {
        // Given: rules without schedule fields (same as existing tests)
        let json = r#"{
            "priority": [
                {"command": "claude", "model": "high", "priority": 42}
            ],
            "agents": [{"command": "claude"}]
        }"#;
        let settings: Settings = serde_json::from_str(json)?;
        let now = make_local_dt(2024, 1, 8, 10);

        // When: calling priority_for_at
        let result =
            settings.priority_for_components_at("claude", Some("claude"), Some("high"), &now);

        // Then: same result as the original priority_for_components
        assert_eq!(result, 42);
        Ok(())
    }

    #[test]
    fn test_priority_for_at_returns_zero_when_no_rule_matches() -> TestResult {
        // Given: rule for different command
        let json = r#"{"priority": [{"command": "codex", "priority": 50}], "agents": []}"#;
        let settings: Settings = serde_json::from_str(json)?;
        let now = make_local_dt(2024, 1, 8, 10);

        // When/Then: no matching rule → 0
        assert_eq!(
            settings.priority_for_components_at("claude", Some("claude"), None, &now),
            0
        );
        Ok(())
    }

    #[test]
    fn test_priority_for_at_scheduled_rule_overrides_base_when_active() -> TestResult {
        // Given: base rule (priority 50) and a higher-priority scheduled rule for Mon-Fri 21-27
        let json = r#"{
            "priority": [
                {"command": "codex", "priority": 50},
                {"command": "codex", "priority": 200, "weekdays": ["1-5"], "hours": ["21-27"]}
            ],
            "agents": []
        }"#;
        let settings: Settings = serde_json::from_str(json)?;
        // 2024-01-08 22:00 = Monday 22:00 → active window
        let active_time = make_local_dt(2024, 1, 8, 22);

        // When: evaluating during active window
        let result =
            settings.priority_for_components_at("codex", Some("codex"), None, &active_time);

        // Then: scheduled rule (200) overrides base rule (50)
        assert_eq!(result, 200);
        Ok(())
    }

    #[test]
    fn test_priority_for_at_base_rule_active_when_scheduled_is_inactive() -> TestResult {
        // Given: same rules as above
        let json = r#"{
            "priority": [
                {"command": "codex", "priority": 50},
                {"command": "codex", "priority": 200, "weekdays": ["1-5"], "hours": ["21-27"]}
            ],
            "agents": []
        }"#;
        let settings: Settings = serde_json::from_str(json)?;
        // 2024-01-13 22:00 = Saturday 22:00 → outside Mon-Fri window
        let inactive_time = make_local_dt(2024, 1, 13, 22);

        // When: evaluating outside active window
        let result =
            settings.priority_for_components_at("codex", Some("codex"), None, &inactive_time);

        // Then: base rule (50) is used
        assert_eq!(result, 50);
        Ok(())
    }

    #[test]
    fn test_priority_for_at_most_specific_rule_wins_over_less_specific() -> TestResult {
        // Given: three rules with different specificity for the same target
        // - base rule (no schedule): priority 10
        // - hours-only rule: priority 30
        // - weekdays+hours rule: priority 50
        let json = r#"{
            "priority": [
                {"command": "codex", "priority": 10},
                {"command": "codex", "priority": 30, "hours": ["21-27"]},
                {"command": "codex", "priority": 50, "weekdays": ["1-5"], "hours": ["21-27"]}
            ],
            "agents": []
        }"#;
        let settings: Settings = serde_json::from_str(json)?;
        // Monday 22:00 → all three rules match
        let monday_22h = make_local_dt(2024, 1, 8, 22);

        // When: evaluating during the window all three match
        let result = settings.priority_for_components_at("codex", Some("codex"), None, &monday_22h);

        // Then: most specific rule (weekdays+hours, priority 50) wins
        assert_eq!(result, 50);
        Ok(())
    }

    #[test]
    fn test_priority_for_at_same_specificity_first_rule_wins() -> TestResult {
        // Given: two rules with same specificity (both hours-only), different priorities
        let json = r#"{
            "priority": [
                {"command": "codex", "priority": 30, "hours": ["20-23"]},
                {"command": "codex", "priority": 99, "hours": ["21-27"]}
            ],
            "agents": []
        }"#;
        let settings: Settings = serde_json::from_str(json)?;
        // 22:00 matches both "20-23" and "21-27"
        let time_22h = make_local_dt(2024, 1, 8, 22);

        // When: both match with equal specificity
        let result = settings.priority_for_components_at("codex", Some("codex"), None, &time_22h);

        // Then: first rule wins (stable, order-compatible)
        assert_eq!(result, 30);
        Ok(())
    }

    #[test]
    fn test_priority_for_at_hours_only_rule_matches_when_weekday_inactive() -> TestResult {
        // Given: base rule and a hours-only rule (no weekday restriction)
        let json = r#"{
            "priority": [
                {"command": "codex", "priority": 10},
                {"command": "codex", "priority": 80, "hours": ["21-27"]}
            ],
            "agents": []
        }"#;
        let settings: Settings = serde_json::from_str(json)?;
        // Saturday 22:00 → hours-only rule applies (no weekday restriction)
        let saturday_22h = make_local_dt(2024, 1, 13, 22);

        // When/Then: hours-only rule (80) wins over base rule (10)
        let result =
            settings.priority_for_components_at("codex", Some("codex"), None, &saturday_22h);
        assert_eq!(result, 80);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Save with schedule fields: JSONC comment preservation
    // -----------------------------------------------------------------------

    #[test]
    fn test_save_preserves_comments_with_weekdays_and_hours_fields() -> TestResult {
        // Given: a JSONC file with comments and a scheduled priority rule
        let jsonc = r#"{
    // Scheduled priority overrides
    "priority": [
        // Base rule
        {"command": "codex", "priority": 50},
        // Nighttime boost
        {"command": "codex", "priority": 200, "weekdays": ["1-5"], "hours": ["21-27"]}
    ],
    "agents": [{"command": "codex"}]
}"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), jsonc)?;

        let mut settings = Settings::load(Some(tmp.path()))?;
        // Modify priority to trigger CST save
        settings.priority[0].priority = 60;
        settings.save(Some(tmp.path()))?;

        let content = std::fs::read_to_string(tmp.path())?;

        // Then: comments are preserved
        assert!(
            content.contains("// Scheduled priority overrides"),
            "top-level comment lost:\n{content}"
        );
        assert!(
            content.contains("// Base rule"),
            "base rule comment lost:\n{content}"
        );
        assert!(
            content.contains("// Nighttime boost"),
            "scheduled rule comment lost:\n{content}"
        );
        // And the updated priority value is present
        assert!(
            content.contains("60"),
            "updated priority value missing:\n{content}"
        );
        // And the schedule fields are preserved
        assert!(content.contains("21-27"), "hours field lost:\n{content}");
        assert!(content.contains("1-5"), "weekdays field lost:\n{content}");
        Ok(())
    }
}
