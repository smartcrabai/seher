use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct UsageWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

impl UsageWindow {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.utilization.unwrap_or(0.0) >= 100.0
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UsageResponse {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_sonnet: Option<UsageWindow>,
    pub seven_day_oauth_apps: Option<UsageWindow>,
    pub seven_day_opus: Option<UsageWindow>,
    pub seven_day_cowork: Option<UsageWindow>,
    pub iguana_necktie: Option<UsageWindow>,
    pub extra_usage: Option<UsageWindow>,
}

impl UsageResponse {
    /// Returns all present windows as `(name, &UsageWindow)` pairs.
    #[must_use]
    pub fn all_windows(&self) -> Vec<(&str, &UsageWindow)> {
        [
            ("five_hour", self.five_hour.as_ref()),
            ("seven_day", self.seven_day.as_ref()),
            ("seven_day_sonnet", self.seven_day_sonnet.as_ref()),
            ("seven_day_oauth_apps", self.seven_day_oauth_apps.as_ref()),
            ("seven_day_opus", self.seven_day_opus.as_ref()),
            ("seven_day_cowork", self.seven_day_cowork.as_ref()),
            ("iguana_necktie", self.iguana_necktie.as_ref()),
            ("extra_usage", self.extra_usage.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, w)| w.map(|w| (name, w)))
        .collect()
    }

    #[must_use]
    pub fn next_reset_time(&self) -> Option<chrono::DateTime<Utc>> {
        self.all_windows()
            .into_iter()
            .filter(|(_, w)| w.is_limited())
            .filter_map(|(_, w)| w.resets_at)
            .max()
    }
}
