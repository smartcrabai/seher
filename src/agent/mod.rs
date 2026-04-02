use crate::Cookie;
use crate::config::AgentConfig;
use chrono::{DateTime, Utc};
use serde::Serialize;

pub struct Agent {
    pub config: AgentConfig,
    pub cookies: Vec<Cookie>,
}

#[derive(Debug)]
pub enum AgentLimit {
    NotLimited,
    Limited { reset_time: Option<DateTime<Utc>> },
}

#[derive(Debug, Serialize)]
pub struct UsageEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub limited: bool,
    pub utilization: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub command: String,
    pub provider: Option<String>,
    pub usage: Vec<UsageEntry>,
}

fn codex_usage_entries(prefix: &str, limit: &crate::codex::CodexRateLimit) -> Vec<UsageEntry> {
    let has_limited_window = [
        limit.primary_window.as_ref(),
        limit.secondary_window.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(crate::codex::types::CodexWindow::is_limited);
    let fallback_reset = if limit.is_limited() && !has_limited_window {
        limit.next_reset_time()
    } else {
        None
    };

    let mut entries = Vec::new();

    for (suffix, window) in [
        ("primary", limit.primary_window.as_ref()),
        ("secondary", limit.secondary_window.as_ref()),
    ] {
        if let Some(window) = window {
            let resets_at = window.reset_at_datetime();
            entries.push(UsageEntry {
                entry_type: format!("{prefix}_{suffix}"),
                limited: window.is_limited()
                    || (fallback_reset.is_some() && resets_at == fallback_reset),
                utilization: window.used_percent,
                resets_at,
            });
        }
    }

    if entries.is_empty() && limit.is_limited() {
        entries.push(UsageEntry {
            entry_type: prefix.to_string(),
            limited: true,
            utilization: 100.0,
            resets_at: limit.next_reset_time(),
        });
    }

    entries
}

impl Agent {
    #[must_use]
    pub fn new(config: AgentConfig, cookies: Vec<Cookie>) -> Self {
        Self { config, cookies }
    }

    #[must_use]
    pub fn command(&self) -> &str {
        &self.config.command
    }

    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.config.args
    }

    /// # Errors
    ///
    /// Returns an error if fetching usage from the provider API fails or the domain is unknown.
    pub async fn check_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        match self.config.resolve_provider() {
            Some("claude") => self.check_claude_limit().await,
            Some("codex") => self.check_codex_limit().await,
            Some("copilot") => self.check_copilot_limit().await,
            Some("openrouter") => self.check_openrouter_limit().await,
            Some("glm") => self.check_glm_limit().await,
            Some("zai") => self.check_zai_limit().await,
            Some("kimi-k2") => self.check_kimik2_limit().await,
            Some("warp") => self.check_warp_limit().await,
            Some("kiro") => self.check_kiro_limit().await,
            Some("opencode-go") => self.check_opencode_go_limit(),
            None => Ok(AgentLimit::NotLimited),
            Some(p) => Err(format!("Unknown provider: {p}").into()),
        }
    }

    /// # Errors
    ///
    /// Returns an error if fetching usage from the provider API fails or the domain is unknown.
    #[expect(clippy::too_many_lines)]
    pub async fn fetch_status(&self) -> Result<AgentStatus, Box<dyn std::error::Error>> {
        let command = self.config.command.clone();
        let provider = self.config.resolve_provider().map(ToString::to_string);
        let usage = match provider.as_deref() {
            None => vec![],
            Some("claude") => {
                let usage = crate::claude::ClaudeClient::fetch_usage(&self.cookies).await?;
                usage
                    .all_windows()
                    .into_iter()
                    .map(|(name, w)| UsageEntry {
                        entry_type: name.to_string(),
                        limited: w.is_limited(),
                        utilization: w.utilization.unwrap_or(0.0),
                        resets_at: w.resets_at,
                    })
                    .collect()
            }
            Some("codex") => {
                let usage = crate::codex::CodexClient::fetch_usage(&self.cookies).await?;
                [
                    ("rate_limit", &usage.rate_limit),
                    ("code_review_rate_limit", &usage.code_review_rate_limit),
                ]
                .into_iter()
                .flat_map(|(prefix, limit)| codex_usage_entries(prefix, limit))
                .collect()
            }
            Some("copilot") => {
                let quota = crate::copilot::CopilotClient::fetch_quota(&self.cookies).await?;
                vec![
                    UsageEntry {
                        entry_type: "chat_utilization".to_string(),
                        limited: quota.chat_utilization >= 100.0,
                        utilization: quota.chat_utilization,
                        resets_at: quota.reset_time,
                    },
                    UsageEntry {
                        entry_type: "premium_utilization".to_string(),
                        limited: quota.premium_utilization >= 100.0,
                        utilization: quota.premium_utilization,
                        resets_at: quota.reset_time,
                    },
                ]
            }
            Some("openrouter") => {
                let management_key = self.openrouter_management_key()?;
                let credits =
                    crate::openrouter::OpenRouterClient::fetch_credits(management_key).await?;
                vec![UsageEntry {
                    entry_type: "credits".to_string(),
                    limited: credits.data.is_limited(),
                    utilization: credits.data.utilization(),
                    resets_at: None,
                }]
            }
            Some("glm") => {
                let api_key = self.glm_api_key()?;
                let quota = crate::glm::GlmClient::fetch_quota(api_key).await?;
                match quota.data {
                    Some(data) => data
                        .limits
                        .iter()
                        .map(|l| UsageEntry {
                            entry_type: l.limit_type.clone(),
                            limited: l.percentage >= 100,
                            utilization: f64::from(l.percentage),
                            resets_at: l.next_reset_time.and_then(DateTime::from_timestamp_millis),
                        })
                        .collect(),
                    None => vec![],
                }
            }
            Some("zai") => {
                let api_key = self.resolve_env_key("Z_AI_API_KEY")?;
                let quota_url = self.resolve_optional_env("Z_AI_QUOTA_URL");
                let quota =
                    crate::zai::ZaiClient::fetch_quota(&api_key, quota_url.as_deref()).await?;
                match quota.data {
                    Some(data) => data
                        .limits
                        .iter()
                        .map(|l| UsageEntry {
                            entry_type: l.limit_type.clone(),
                            limited: l.percentage >= 100,
                            utilization: f64::from(l.percentage),
                            resets_at: l.next_reset_time.and_then(DateTime::from_timestamp_millis),
                        })
                        .collect(),
                    None => vec![],
                }
            }
            Some("kimi-k2") => {
                let api_key = self.resolve_env_key("KIMI_K2_API_KEY")?;
                let credits = crate::kimik2::KimiK2Client::fetch_credits(&api_key).await?;
                vec![UsageEntry {
                    entry_type: "credits".to_string(),
                    limited: credits.is_limited(),
                    utilization: credits.utilization(),
                    resets_at: None,
                }]
            }
            Some("warp") => {
                let api_key = self.resolve_env_key("WARP_API_KEY")?;
                let info = crate::warp::WarpClient::fetch_limit_info(&api_key).await?;
                let limit_info = &info.data.get_request_limit_info;
                vec![UsageEntry {
                    entry_type: "requests".to_string(),
                    limited: limit_info.is_limited(),
                    utilization: limit_info.utilization(),
                    resets_at: Self::reset_time_from_seconds(limit_info.reset_in_seconds),
                }]
            }
            Some("kiro") => {
                let info = crate::kiro::KiroClient::fetch_usage().await?;
                vec![UsageEntry {
                    entry_type: "requests".to_string(),
                    limited: info.is_limited(),
                    utilization: info.utilization(),
                    resets_at: Self::reset_time_from_seconds(info.reset_in_seconds),
                }]
            }
            Some("opencode-go") => self
                .opencode_go_usage_snapshot()?
                .windows
                .into_iter()
                .map(|window| UsageEntry {
                    entry_type: window.entry_type.to_string(),
                    limited: window.is_limited(),
                    utilization: window.utilization(),
                    resets_at: window.resets_at,
                })
                .collect(),
            Some(p) => return Err(format!("Unknown provider: {p}").into()),
        };
        Ok(AgentStatus {
            command,
            provider,
            usage,
        })
    }

    async fn check_claude_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let usage = crate::claude::ClaudeClient::fetch_usage(&self.cookies).await?;
        let windows = usage.all_windows();

        let (has_limited, reset_time) =
            windows
                .iter()
                .fold((false, None), |(has_lim, max_t), (_, w)| {
                    if w.is_limited() {
                        (true, max_t.max(w.resets_at))
                    } else {
                        (has_lim, max_t)
                    }
                });

        if has_limited {
            Ok(AgentLimit::Limited { reset_time })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    async fn check_copilot_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let quota = crate::copilot::CopilotClient::fetch_quota(&self.cookies).await?;

        if quota.is_limited() {
            Ok(AgentLimit::Limited {
                reset_time: quota.reset_time,
            })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    fn openrouter_management_key(&self) -> Result<&str, Box<dyn std::error::Error>> {
        self.config
            .openrouter_management_key
            .as_deref()
            .ok_or_else(|| {
                "openrouter_management_key is required for OpenRouter provider"
                    .to_string()
                    .into()
            })
    }

    async fn check_openrouter_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let management_key = self.openrouter_management_key()?;
        let credits = crate::openrouter::OpenRouterClient::fetch_credits(management_key).await?;
        if credits.data.is_limited() {
            Ok(AgentLimit::Limited { reset_time: None })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    fn glm_api_key(&self) -> Result<&str, Box<dyn std::error::Error>> {
        self.config.glm_api_key.as_deref().ok_or_else(|| {
            "glm_api_key is required for GLM provider"
                .to_string()
                .into()
        })
    }

    async fn check_glm_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let api_key = self.glm_api_key()?;
        let quota = crate::glm::GlmClient::fetch_quota(api_key).await?;
        match quota.data {
            Some(data) if data.is_limited() => {
                let reset_time = data
                    .limits
                    .iter()
                    .filter_map(|l| l.next_reset_time)
                    .filter_map(DateTime::from_timestamp_millis)
                    .max();
                Ok(AgentLimit::Limited { reset_time })
            }
            _ => Ok(AgentLimit::NotLimited),
        }
    }

    fn reset_time_from_seconds(secs: Option<i64>) -> Option<DateTime<Utc>> {
        secs.and_then(|s| Utc::now().checked_add_signed(chrono::Duration::seconds(s)))
    }

    fn resolve_env_key(&self, key: &str) -> Result<String, Box<dyn std::error::Error>> {
        // 1. Check agent config env
        if let Some(env) = &self.config.env
            && let Some(val) = env.get(key)
        {
            return Ok(val.clone());
        }
        // 2. Check process environment
        if let Ok(val) = std::env::var(key) {
            return Ok(val);
        }
        Err(format!("{key} is required for this provider").into())
    }

    fn resolve_optional_env(&self, key: &str) -> Option<String> {
        self.config
            .env
            .as_ref()
            .and_then(|env| env.get(key).cloned())
            .or_else(|| std::env::var(key).ok())
    }

    async fn check_zai_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let api_key = self.resolve_env_key("Z_AI_API_KEY")?;
        let quota_url = self.resolve_optional_env("Z_AI_QUOTA_URL");
        let quota = crate::zai::ZaiClient::fetch_quota(&api_key, quota_url.as_deref()).await?;
        match quota.data {
            Some(data) if data.is_limited() => {
                let reset_time = data
                    .limits
                    .iter()
                    .filter_map(|l| l.next_reset_time)
                    .filter_map(DateTime::from_timestamp_millis)
                    .max();
                Ok(AgentLimit::Limited { reset_time })
            }
            _ => Ok(AgentLimit::NotLimited),
        }
    }

    async fn check_kimik2_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let api_key = self.resolve_env_key("KIMI_K2_API_KEY")?;
        let credits = crate::kimik2::KimiK2Client::fetch_credits(&api_key).await?;
        if credits.is_limited() {
            Ok(AgentLimit::Limited { reset_time: None })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    async fn check_warp_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let api_key = self.resolve_env_key("WARP_API_KEY")?;
        let info = crate::warp::WarpClient::fetch_limit_info(&api_key).await?;
        let limit_info = &info.data.get_request_limit_info;
        if limit_info.is_limited() {
            Ok(AgentLimit::Limited {
                reset_time: Self::reset_time_from_seconds(limit_info.reset_in_seconds),
            })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    async fn check_kiro_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let info = crate::kiro::KiroClient::fetch_usage().await?;
        if info.is_limited() {
            Ok(AgentLimit::Limited {
                reset_time: Self::reset_time_from_seconds(info.reset_in_seconds),
            })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    fn check_opencode_go_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let snapshot = self.opencode_go_usage_snapshot()?;
        if snapshot
            .windows
            .iter()
            .any(crate::opencode_go::OpencodeGoUsageWindow::is_limited)
        {
            Ok(AgentLimit::Limited {
                reset_time: snapshot.reset_time(),
            })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    async fn check_codex_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        let usage = crate::codex::CodexClient::fetch_usage(&self.cookies).await?;

        if usage.rate_limit.is_limited() {
            Ok(AgentLimit::Limited {
                reset_time: usage.rate_limit.next_reset_time(),
            })
        } else {
            Ok(AgentLimit::NotLimited)
        }
    }

    /// # Errors
    ///
    /// Returns an error if spawning or waiting on the child process fails.
    pub fn execute(
        &self,
        resolved_args: &[String],
        extra_args: &[String],
    ) -> std::io::Result<std::process::ExitStatus> {
        if let Some((cmd, args)) = self.config.pre_command.split_first() {
            let mut pre_cmd = std::process::Command::new(cmd);
            pre_cmd.args(args);
            if let Some(env) = &self.config.env {
                pre_cmd.envs(env);
            }
            let status = pre_cmd.status()?;
            if !status.success() {
                return Ok(status);
            }
        }
        let mut cmd = std::process::Command::new(self.command());
        cmd.args(resolved_args);
        cmd.args(extra_args);
        if let Some(env) = &self.config.env {
            cmd.envs(env);
        }
        cmd.status()
    }

    #[must_use]
    pub fn has_model(&self, model_key: &str) -> bool {
        self.config.has_model(model_key)
    }

    #[must_use]
    pub fn resolved_args(&self, model: Option<&str>) -> Vec<String> {
        const MODEL_PLACEHOLDER: &str = "{model}";
        let mut args: Vec<String> = self
            .config
            .args
            .iter()
            .filter_map(|arg| {
                if arg.contains(MODEL_PLACEHOLDER) {
                    let model_key = model?;
                    let replacement = self
                        .config
                        .models
                        .as_ref()
                        .and_then(|m| m.get(model_key))
                        .map_or(model_key, |s| s.as_str());
                    Some(arg.replace(MODEL_PLACEHOLDER, replacement))
                } else {
                    Some(arg.clone())
                }
            })
            .collect();

        // If models map is not set, pass --model <value> through as-is
        if self.config.models.is_none()
            && let Some(model_key) = model
        {
            args.push("--model".to_string());
            args.push(model_key.to_string());
        }

        args
    }

    #[must_use]
    pub fn mapped_args(&self, args: &[String]) -> Vec<String> {
        args.iter()
            .flat_map(|arg| {
                self.config
                    .arg_maps
                    .get(arg.as_str())
                    .map_or_else(|| std::slice::from_ref(arg), Vec::as_slice)
            })
            .cloned()
            .collect()
    }

    fn opencode_go_usage_snapshot(
        &self,
    ) -> Result<crate::opencode_go::OpencodeGoUsageSnapshot, Box<dyn std::error::Error>> {
        let db_path = self.resolve_optional_env("SEHER_OPENCODE_DB_PATH");
        let auth_path = self.resolve_optional_env("SEHER_OPENCODE_AUTH_PATH");
        Ok(
            crate::opencode_go::OpencodeGoUsageStore::fetch_usage_with_paths_at(
                db_path.as_deref().map(std::path::Path::new),
                auth_path.as_deref().map(std::path::Path::new),
                Utc::now(),
            )?,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::codex::{CodexRateLimit, CodexWindow};
    use crate::config::AgentConfig;

    fn make_agent(
        models: Option<HashMap<String, String>>,
        arg_maps: HashMap<String, Vec<String>>,
    ) -> Agent {
        Agent::new(
            AgentConfig {
                command: "claude".to_string(),
                args: vec![],
                models,
                arg_maps,
                env: None,
                provider: None,
                openrouter_management_key: None,
                glm_api_key: None,
                pre_command: vec![],
            },
            vec![],
        )
    }

    #[test]
    fn has_model_returns_true_when_models_is_none() {
        let agent = make_agent(None, HashMap::new());
        assert!(agent.has_model("high"));
        assert!(agent.has_model("anything"));
    }

    #[test]
    fn resolved_args_passthrough_when_models_is_none_with_model() {
        let agent = make_agent(None, HashMap::new());
        let args = agent.resolved_args(Some("high"));
        assert_eq!(args, vec!["--model", "high"]);
    }

    #[test]
    fn resolved_args_no_model_flag_when_models_is_none_without_model() {
        let agent = make_agent(None, HashMap::new());
        let args = agent.resolved_args(None);
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn mapped_args_passthrough_when_arg_maps_is_empty() {
        let agent = make_agent(None, HashMap::new());
        let args = vec!["--danger".to_string(), "fix bugs".to_string()];

        assert_eq!(agent.mapped_args(&args), args);
    }

    #[test]
    fn mapped_args_replaces_matching_tokens() {
        let mut arg_maps = HashMap::new();
        arg_maps.insert("--danger".to_string(), vec!["--yolo".to_string()]);
        let agent = make_agent(None, arg_maps);

        assert_eq!(
            agent.mapped_args(&["--danger".to_string(), "fix bugs".to_string()]),
            vec!["--yolo".to_string(), "fix bugs".to_string()]
        );
    }

    #[test]
    fn mapped_args_can_expand_to_multiple_tokens() {
        let mut arg_maps = HashMap::new();
        arg_maps.insert(
            "--danger".to_string(),
            vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
            ],
        );
        let agent = make_agent(None, arg_maps);

        assert_eq!(
            agent.mapped_args(&["--danger".to_string(), "fix bugs".to_string()]),
            vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
                "fix bugs".to_string(),
            ]
        );
    }

    #[test]
    fn codex_usage_entries_marks_blocking_window_when_only_top_level_limit_is_set() {
        let limit = CodexRateLimit {
            allowed: false,
            limit_reached: false,
            primary_window: Some(CodexWindow {
                used_percent: 55.0,
                limit_window_seconds: 60,
                reset_after_seconds: 30,
                reset_at: 100,
            }),
            secondary_window: Some(CodexWindow {
                used_percent: 40.0,
                limit_window_seconds: 120,
                reset_after_seconds: 90,
                reset_at: 200,
            }),
        };

        let entries = codex_usage_entries("rate_limit", &limit);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, "rate_limit_primary");
        assert!(!entries[0].limited);
        assert_eq!(entries[1].entry_type, "rate_limit_secondary");
        assert!(entries[1].limited);
    }

    #[test]
    fn codex_usage_entries_adds_summary_when_limit_has_no_windows() {
        let limit = CodexRateLimit {
            allowed: false,
            limit_reached: true,
            primary_window: None,
            secondary_window: None,
        };

        let entries = codex_usage_entries("code_review_rate_limit", &limit);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, "code_review_rate_limit");
        assert!(entries[0].limited);
        assert!((entries[0].utilization - 100.0).abs() < f64::EPSILON);
        assert_eq!(entries[0].resets_at, None);
    }

    // -----------------------------------------------------------------------
    // OpenRouter dispatch tests
    // These tests verify that check_limit() / fetch_status() correctly route
    // to the openrouter handler when provider == "openrouter", and that a
    // missing management key causes an immediate error (no HTTP call made).
    // -----------------------------------------------------------------------

    fn make_openrouter_agent(management_key: Option<&str>) -> Agent {
        Agent::new(
            AgentConfig {
                command: "myai".to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
                provider: Some(crate::config::ProviderConfig::Explicit(
                    "openrouter".to_string(),
                )),
                openrouter_management_key: management_key.map(str::to_string),
                glm_api_key: None,
                pre_command: vec![],
            },
            vec![],
        )
    }

    fn make_agent_with_pre_command(pre_command: Vec<String>, main_command: &str) -> Agent {
        Agent::new(
            AgentConfig {
                command: main_command.to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
                provider: None,
                openrouter_management_key: None,
                glm_api_key: None,
                pre_command,
            },
            vec![],
        )
    }

    #[test]
    #[cfg(unix)]
    fn execute_runs_main_command_when_pre_command_succeeds() -> TestResult {
        // pre_command: true (always exits 0), main: true
        let agent = make_agent_with_pre_command(vec!["true".to_string()], "true");
        let status = agent.execute(&[], &[])?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn execute_skips_main_command_when_pre_command_fails() -> TestResult {
        // pre_command: false (always exits non-0), main: true
        let agent = make_agent_with_pre_command(vec!["false".to_string()], "true");
        let status = agent.execute(&[], &[])?;
        assert!(!status.success());
        Ok(())
    }

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_openrouter_returns_error_when_management_key_is_missing() -> TestResult {
        // Given: openrouter agent with no management key configured
        let agent = make_openrouter_agent(None);

        // When: check_limit is called
        let result = agent.check_limit().await;

        // Then: error mentions the missing key -- no HTTP call should be made
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(err_msg.contains("openrouter_management_key"));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_openrouter_returns_error_when_management_key_is_missing() -> TestResult {
        // Given: openrouter agent with no management key configured
        let agent = make_openrouter_agent(None);

        // When: fetch_status is called
        let result = agent.fetch_status().await;

        // Then: error mentions the missing key -- no HTTP call should be made
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(err_msg.contains("openrouter_management_key"));
        Ok(())
    }

    fn make_api_key_agent(provider: &str) -> Agent {
        Agent::new(
            AgentConfig {
                command: "myai".to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
                provider: Some(crate::config::ProviderConfig::Explicit(
                    provider.to_string(),
                )),
                openrouter_management_key: None,
                glm_api_key: None,
                pre_command: vec![],
            },
            vec![],
        )
    }

    // -- zai --

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_zai_returns_error_when_api_key_is_missing() -> TestResult {
        let agent = make_api_key_agent("zai");
        let result = agent.check_limit().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(
            err_msg.contains("Z_AI_API_KEY"),
            "error should mention Z_AI_API_KEY, got: {err_msg}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_zai_returns_error_when_api_key_is_missing() -> TestResult {
        let agent = make_api_key_agent("zai");
        let result = agent.fetch_status().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(
            err_msg.contains("Z_AI_API_KEY"),
            "error should mention Z_AI_API_KEY, got: {err_msg}"
        );
        Ok(())
    }

    // -- kimi-k2 --

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_kimik2_returns_error_when_api_key_is_missing() -> TestResult {
        let agent = make_api_key_agent("kimi-k2");
        let result = agent.check_limit().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(
            err_msg.contains("KIMI_K2_API_KEY"),
            "error should mention KIMI_K2_API_KEY, got: {err_msg}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_kimik2_returns_error_when_api_key_is_missing() -> TestResult {
        let agent = make_api_key_agent("kimi-k2");
        let result = agent.fetch_status().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(
            err_msg.contains("KIMI_K2_API_KEY"),
            "error should mention KIMI_K2_API_KEY, got: {err_msg}"
        );
        Ok(())
    }

    // -- warp --

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_warp_returns_error_when_api_key_is_missing() -> TestResult {
        let agent = make_api_key_agent("warp");
        let result = agent.check_limit().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(
            err_msg.contains("WARP_API_KEY"),
            "error should mention WARP_API_KEY, got: {err_msg}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_warp_returns_error_when_api_key_is_missing() -> TestResult {
        let agent = make_api_key_agent("warp");
        let result = agent.fetch_status().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(
            err_msg.contains("WARP_API_KEY"),
            "error should mention WARP_API_KEY, got: {err_msg}"
        );
        Ok(())
    }

    // -- kiro (CLI-based, no API key needed for dispatch, but must not panic) --

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_kiro_returns_error_when_command_not_found() -> TestResult {
        let agent = make_api_key_agent("kiro");
        let result = agent.check_limit().await;
        assert!(result.is_err(), "kiro without CLI should return an error");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_kiro_returns_error_when_command_not_found() -> TestResult {
        let agent = make_api_key_agent("kiro");
        let result = agent.fetch_status().await;
        assert!(result.is_err(), "kiro without CLI should return an error");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_opencode_go_uses_local_history() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute("CREATE TABLE message (data TEXT NOT NULL)", [])?;
        conn.execute(
            "INSERT INTO message (data) VALUES (?1)",
            [r#"{"role":"assistant","providerID":"opencode-go","cost":6.5,"time":{"completed":4102448400000}}"#],
        )?;
        conn.execute(
            "INSERT INTO message (data) VALUES (?1)",
            [r#"{"role":"assistant","providerID":"opencode-go","cost":6.0,"time":{"completed":4102461000000}}"#],
        )?;
        drop(conn);

        let mut agent = make_api_key_agent("opencode-go");
        agent.config.env = Some(HashMap::from([(
            "SEHER_OPENCODE_DB_PATH".to_string(),
            db_path.display().to_string(),
        )]));
        let result = agent.check_limit().await?;
        assert!(matches!(result, AgentLimit::Limited { .. }));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_opencode_go_returns_usage_windows() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute("CREATE TABLE message (data TEXT NOT NULL)", [])?;
        conn.execute(
            "INSERT INTO message (data) VALUES (?1)",
            [r#"{"role":"assistant","providerID":"opencode-go","cost":2.25,"time":{"completed":4102461000000}}"#],
        )?;
        drop(conn);

        let mut agent = make_api_key_agent("opencode-go");
        agent.config.env = Some(HashMap::from([(
            "SEHER_OPENCODE_DB_PATH".to_string(),
            db_path.display().to_string(),
        )]));
        let status = agent.fetch_status().await?;
        assert_eq!(status.provider.as_deref(), Some("opencode-go"));
        assert_eq!(status.usage.len(), 3);
        assert!(
            status
                .usage
                .iter()
                .any(|entry| entry.entry_type == "five_hour_spend")
        );
        assert!(
            status
                .usage
                .iter()
                .any(|entry| entry.entry_type == "weekly_spend")
        );
        assert!(
            status
                .usage
                .iter()
                .any(|entry| entry.entry_type == "monthly_spend")
        );
        Ok(())
    }

    // -- unknown provider still errors --

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_unknown_provider_returns_error() -> TestResult {
        let agent = make_api_key_agent("nonexistent-provider");
        let result = agent.check_limit().await;
        let err_msg = result.err().ok_or("expected Err")?.to_string();
        assert!(err_msg.contains("Unknown provider"), "got: {err_msg}");
        Ok(())
    }
}
