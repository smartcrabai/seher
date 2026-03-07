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
    pub usage: Vec<UsageEntry>,
}

impl Agent {
    pub fn new(config: AgentConfig, cookies: Vec<Cookie>) -> Self {
        Self { config, cookies }
    }

    pub fn command(&self) -> &str {
        &self.config.command
    }

    pub fn args(&self) -> &[String] {
        &self.config.args
    }

    pub async fn check_limit(&self) -> Result<AgentLimit, Box<dyn std::error::Error>> {
        match self.config.resolve_domain() {
            Some("claude.ai") => self.check_claude_limit().await,
            Some("github.com") => self.check_copilot_limit().await,
            None => Ok(AgentLimit::NotLimited),
            Some(d) => Err(format!("Unknown domain: {}", d).into()),
        }
    }

    pub async fn fetch_status(&self) -> Result<AgentStatus, Box<dyn std::error::Error>> {
        match self.config.resolve_domain() {
            None => Ok(AgentStatus {
                command: self.config.command.clone(),
                usage: vec![],
            }),
            Some("claude.ai") => {
                let usage = crate::claude::ClaudeClient::fetch_usage(&self.cookies).await?;
                let windows = [
                    ("five_hour", &usage.five_hour),
                    ("seven_day", &usage.seven_day),
                    ("seven_day_sonnet", &usage.seven_day_sonnet),
                ];
                let entries = windows
                    .into_iter()
                    .filter_map(|(name, w)| {
                        w.as_ref().map(|w| UsageEntry {
                            entry_type: name.to_string(),
                            limited: w.utilization >= 100.0,
                            utilization: w.utilization,
                            resets_at: w.resets_at,
                        })
                    })
                    .collect();
                Ok(AgentStatus {
                    command: self.config.command.clone(),
                    usage: entries,
                })
            }
            Some("github.com") => {
                let quota = crate::copilot::CopilotClient::fetch_quota(&self.cookies).await?;
                let entries = vec![
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
                ];
                Ok(AgentStatus {
                    command: self.config.command.clone(),
                    usage: entries,
                })
            }
            Some(d) => Err(format!("Unknown domain: {}", d).into()),
        }
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

    pub fn has_model(&self, model_key: &str) -> bool {
        match &self.config.models {
            None => true, // no models map → pass-through, accepts any model key
            Some(m) => m.contains_key(model_key),
        }
    }

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
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::AgentConfig;

    fn make_agent(models: Option<HashMap<String, String>>) -> Agent {
        Agent::new(
            AgentConfig {
                command: "claude".to_string(),
                args: vec![],
                models,
                env: None,
                provider: None,
            },
            vec![],
        )
    }

    #[test]
    fn has_model_returns_true_when_models_is_none() {
        let agent = make_agent(None);
        assert!(agent.has_model("high"));
        assert!(agent.has_model("anything"));
    }

    #[test]
    fn resolved_args_passthrough_when_models_is_none_with_model() {
        let agent = make_agent(None);
        let args = agent.resolved_args(Some("high"));
        assert_eq!(args, vec!["--model", "high"]);
    }

    #[test]
    fn resolved_args_no_model_flag_when_models_is_none_without_model() {
        let agent = make_agent(None);
        let args = agent.resolved_args(None);
        assert!(!args.contains(&"--model".to_string()));
    }
}
