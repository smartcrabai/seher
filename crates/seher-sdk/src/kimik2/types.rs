use serde::Deserialize;

/// Response from `GET https://kimi-k2.ai/api/user/credits`.
///
/// Kimi-K2 uses a credit-based model with no reset cycle — once credits are
/// consumed they do not replenish automatically.
#[derive(Debug, Deserialize)]
pub struct KimiK2CreditsResponse {
    pub credits: f64,
    pub used: f64,
}

impl KimiK2CreditsResponse {
    /// Returns `true` when all credits have been consumed.
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.used >= self.credits
    }

    /// Returns usage as a percentage (0–100). Returns 100.0 when credits are
    /// zero or negative.
    #[must_use]
    pub fn utilization(&self) -> f64 {
        if self.credits > 0.0 {
            self.used / self.credits * 100.0
        } else {
            100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn test_deserialize_credits_response() -> TestResult {
        let json = r#"{"credits": 100.0, "used": 30.5}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!((response.credits - 100.0).abs() < f64::EPSILON);
        assert!((response.used - 30.5).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_is_not_limited_when_credits_remaining() -> TestResult {
        let json = r#"{"credits": 100.0, "used": 50.0}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!(!response.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_credits_exactly_consumed() -> TestResult {
        let json = r#"{"credits": 50.0, "used": 50.0}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!(response.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_used_exceeds_credits() -> TestResult {
        let json = r#"{"credits": 10.0, "used": 15.0}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!(response.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_zero_credits() -> TestResult {
        let json = r#"{"credits": 0.0, "used": 0.0}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!(response.is_limited());
        Ok(())
    }

    #[test]
    fn test_utilization_computed_correctly() -> TestResult {
        let json = r#"{"credits": 200.0, "used": 50.0}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!((response.utilization() - 25.0).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_utilization_returns_100_when_zero_credits() -> TestResult {
        let json = r#"{"credits": 0.0, "used": 0.0}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!((response.utilization() - 100.0).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_integer_values_deserialized_as_floats() -> TestResult {
        let json = r#"{"credits": 10, "used": 3}"#;

        let response: KimiK2CreditsResponse = serde_json::from_str(json)?;
        assert!((response.credits - 10.0).abs() < f64::EPSILON);
        assert!((response.used - 3.0).abs() < f64::EPSILON);
        Ok(())
    }
}
