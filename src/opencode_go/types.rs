use chrono::{DateTime, Utc};

const LIMIT_EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpencodeGoUsageSource {
    LocalDatabase,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpencodeGoUsageWindow {
    pub entry_type: &'static str,
    pub spent_usd: f64,
    pub limit_usd: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl OpencodeGoUsageWindow {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.spent_usd + LIMIT_EPSILON >= self.limit_usd
    }

    #[must_use]
    pub fn utilization(&self) -> f64 {
        if self.limit_usd <= 0.0 {
            100.0
        } else {
            self.spent_usd / self.limit_usd * 100.0
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpencodeGoUsageSnapshot {
    pub source: OpencodeGoUsageSource,
    pub credentials_available: bool,
    pub total_messages: usize,
    pub windows: Vec<OpencodeGoUsageWindow>,
}

impl OpencodeGoUsageSnapshot {
    #[must_use]
    pub fn reset_time(&self) -> Option<DateTime<Utc>> {
        self.windows
            .iter()
            .filter(|window| window.is_limited())
            .filter_map(|window| window.resets_at)
            .max()
    }
}
