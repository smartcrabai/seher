use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UsageResponse {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_sonnet: Option<UsageWindow>,
}

impl UsageResponse {
    #[must_use]
    pub fn next_reset_time(&self) -> Option<chrono::DateTime<Utc>> {
        [
            self.five_hour.as_ref(),
            self.seven_day.as_ref(),
            self.seven_day_sonnet.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|w| w.utilization >= 100.0)
        .filter_map(|w| w.resets_at)
        .max()
    }
}
