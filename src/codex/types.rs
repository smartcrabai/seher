use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CodexWindow {
    pub used_percent: f64,
    pub limit_window_seconds: i64,
    pub reset_after_seconds: i64,
    pub reset_at: i64,
}

impl CodexWindow {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.used_percent >= 100.0
    }

    #[must_use]
    pub fn reset_at_datetime(&self) -> Option<DateTime<Utc>> {
        Utc.timestamp_opt(self.reset_at, 0).single()
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CodexRateLimit {
    pub allowed: bool,
    pub limit_reached: bool,
    pub primary_window: Option<CodexWindow>,
    pub secondary_window: Option<CodexWindow>,
}

impl CodexRateLimit {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        !self.allowed
            || self.limit_reached
            || [self.primary_window.as_ref(), self.secondary_window.as_ref()]
                .into_iter()
                .flatten()
                .any(CodexWindow::is_limited)
    }

    #[must_use]
    pub fn next_reset_time(&self) -> Option<DateTime<Utc>> {
        [self.primary_window.as_ref(), self.secondary_window.as_ref()]
            .into_iter()
            .flatten()
            .filter(|window| !self.allowed || self.limit_reached || window.is_limited())
            .filter_map(CodexWindow::reset_at_datetime)
            .max()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CodexCredits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: String,
    pub approx_local_messages: Vec<i64>,
    pub approx_cloud_messages: Vec<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CodexUsageResponse {
    pub user_id: String,
    pub account_id: String,
    pub email: String,
    pub plan_type: String,
    pub rate_limit: CodexRateLimit,
    pub code_review_rate_limit: CodexRateLimit,
    pub additional_rate_limits: Option<serde_json::Value>,
    pub credits: CodexCredits,
    pub promo: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_response_and_keeps_unlimited_state() -> Result<(), serde_json::Error> {
        let json = r#"
        {
          "user_id": "user-1",
          "account_id": "user-1",
          "email": "user@example.com",
          "plan_type": "plus",
          "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
              "used_percent": 6,
              "limit_window_seconds": 18000,
              "reset_after_seconds": 13837,
              "reset_at": 1773200619
            },
            "secondary_window": {
              "used_percent": 2,
              "limit_window_seconds": 604800,
              "reset_after_seconds": 600637,
              "reset_at": 1773787419
            }
          },
          "code_review_rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
              "used_percent": 0,
              "limit_window_seconds": 604800,
              "reset_after_seconds": 604800,
              "reset_at": 1773791583
            },
            "secondary_window": null
          },
          "additional_rate_limits": null,
          "credits": {
            "has_credits": false,
            "unlimited": false,
            "balance": "0",
            "approx_local_messages": [0, 0],
            "approx_cloud_messages": [0, 0]
          },
          "promo": null
        }"#;

        let usage: CodexUsageResponse = serde_json::from_str(json)?;

        assert!(!usage.rate_limit.is_limited());
        assert_eq!(usage.rate_limit.next_reset_time(), None);
        let primary_used = usage
            .code_review_rate_limit
            .primary_window
            .as_ref()
            .map(|w| w.used_percent);
        assert_eq!(primary_used, Some(0.0));
        Ok(())
    }

    #[test]
    fn picks_latest_reset_when_multiple_windows_are_limited() {
        let limit = CodexRateLimit {
            allowed: false,
            limit_reached: true,
            primary_window: Some(CodexWindow {
                used_percent: 100.0,
                limit_window_seconds: 18000,
                reset_after_seconds: 100,
                reset_at: 1_773_200_619,
            }),
            secondary_window: Some(CodexWindow {
                used_percent: 100.0,
                limit_window_seconds: 604_800,
                reset_after_seconds: 200,
                reset_at: 1_773_787_419,
            }),
        };

        assert!(limit.is_limited());
        assert_eq!(
            limit.next_reset_time().map(|time| time.timestamp()),
            Some(1_773_787_419)
        );
    }

    #[test]
    fn uses_reset_time_when_allowed_flag_blocks_usage_before_window_hits_100_percent() {
        let limit = CodexRateLimit {
            allowed: false,
            limit_reached: false,
            primary_window: Some(CodexWindow {
                used_percent: 85.0,
                limit_window_seconds: 18000,
                reset_after_seconds: 100,
                reset_at: 1_773_200_619,
            }),
            secondary_window: Some(CodexWindow {
                used_percent: 20.0,
                limit_window_seconds: 604_800,
                reset_after_seconds: 200,
                reset_at: 1_773_787_419,
            }),
        };

        assert!(limit.is_limited());
        assert_eq!(
            limit.next_reset_time().map(|time| time.timestamp()),
            Some(1_773_787_419)
        );
    }
}
