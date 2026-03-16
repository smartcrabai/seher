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
            None => Ok(AgentLimit::NotLimited),
            Some(p) => Err(format!("Unknown provider: {p}").into()),
        }
    }

    /// # Errors
    ///
    /// Returns an error if fetching usage from the provider API fails or the domain is unknown.
    pub async fn fetch_status(&self) -> Result<AgentStatus, Box<dyn std::error::Error>> {
        let command = self.config.command.clone();
        let provider = self.config.resolve_provider().map(ToString::to_string);
        let usage = match provider.as_deref() {
            None => vec![],
            Some("claude") => {
                let usage = crate::claude::ClaudeClient::fetch_usage(&self.cookies).await?;
                let windows = [
                    ("five_hour", &usage.five_hour),
                    ("seven_day", &usage.seven_day),
                    ("seven_day_sonnet", &usage.seven_day_sonnet),
                ];
                windows
                    .into_iter()
                    .filter_map(|(name, w)| {
                        w.as_ref().map(|w| UsageEntry {
                            entry_type: name.to_string(),
                            limited: w.utilization >= 100.0,
                            utilization: w.utilization,
                            resets_at: w.resets_at,
                        })
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

        if let Some(reset_time) = usage.next_reset_time() {
            Ok(AgentLimit::Limited {
                reset_time: Some(reset_time),
            })
        } else {
            let is_limited = [
                usage.five_hour.as_ref(),
                usage.seven_day.as_ref(),
                usage.seven_day_sonnet.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|w| w.utilization >= 100.0);

            if is_limited {
                Ok(AgentLimit::Limited { reset_time: None })
            } else {
                Ok(AgentLimit::NotLimited)
            }
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
        match &self.config.models {
            None => true, // no models map → pass-through, accepts any model key
            Some(m) => m.contains_key(model_key),
        }
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
                provider: Some(crate::config::ProviderConfig::Explicit("openrouter".to_string())),
                openrouter_management_key: management_key.map(str::to_string),
            },
            vec![],
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn check_limit_openrouter_returns_error_when_management_key_is_missing() {
        // Given: openrouter agent with no management key configured
        let agent = make_openrouter_agent(None);

        // When: check_limit is called
        let result = agent.check_limit().await;

        // Then: error mentions the missing key — no HTTP call should be made
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("openrouter_management_key")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_status_openrouter_returns_error_when_management_key_is_missing() {
        // Given: openrouter agent with no management key configured
        let agent = make_openrouter_agent(None);

        // When: fetch_status is called
        let result = agent.fetch_status().await;

        // Then: error mentions the missing key — no HTTP call should be made
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("openrouter_management_key")
        );
    }
}
