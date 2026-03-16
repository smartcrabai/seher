use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct CreditsData {
    pub total_credits: f64,
    pub total_usage: f64,
}

impl CreditsData {
    pub fn is_limited(&self) -> bool {
        self.total_usage >= self.total_credits
    }

    pub fn utilization(&self) -> f64 {
        if self.total_credits > 0.0 {
            self.total_usage / self.total_credits * 100.0
        } else {
            100.0
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreditsResponse {
    pub data: CreditsData,
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn test_deserialize_credits_response_with_positive_balance() -> TestResult {
        // Given: valid API response with credits remaining
        let json = r#"{"data": {"total_credits": 10.0, "total_usage": 5.0}}"#;

        // When: parsed
        let response: CreditsResponse = serde_json::from_str(json)?;

        // Then: fields are correctly deserialized
        assert!((response.data.total_credits - 10.0).abs() < f64::EPSILON);
        assert!((response.data.total_usage - 5.0).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_is_not_limited_when_usage_is_less_than_credits() -> TestResult {
        // Given: response where usage is less than credits
        let json = r#"{"data": {"total_credits": 10.0, "total_usage": 3.0}}"#;

        // When: parsed
        let response: CreditsResponse = serde_json::from_str(json)?;

        // Then: agent is not limited
        assert!(!response.data.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_usage_equals_credits() -> TestResult {
        // Given: response where all credits are exactly consumed
        let json = r#"{"data": {"total_credits": 5.0, "total_usage": 5.0}}"#;

        // When: parsed
        let response: CreditsResponse = serde_json::from_str(json)?;

        // Then: agent is limited
        assert!(response.data.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_usage_exceeds_credits() -> TestResult {
        // Given: response where usage exceeds credits (overrun)
        let json = r#"{"data": {"total_credits": 5.0, "total_usage": 7.5}}"#;

        // When: parsed
        let response: CreditsResponse = serde_json::from_str(json)?;

        // Then: agent is limited
        assert!(response.data.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_zero_credits() -> TestResult {
        // Given: free-tier or exhausted account with no credits at all
        let json = r#"{"data": {"total_credits": 0.0, "total_usage": 0.0}}"#;

        // When: parsed
        let response: CreditsResponse = serde_json::from_str(json)?;

        // Then: agent is limited
        assert!(response.data.is_limited());
        Ok(())
    }

    #[test]
    fn test_deserialize_credits_response_integer_values_as_floats() -> TestResult {
        // Given: API may return integers where floats are expected
        let json = r#"{"data": {"total_credits": 10, "total_usage": 0}}"#;

        // When: parsed
        let response: CreditsResponse = serde_json::from_str(json)?;

        // Then: values are correctly read as f64
        assert!((response.data.total_credits - 10.0).abs() < f64::EPSILON);
        assert!((response.data.total_usage - 0.0).abs() < f64::EPSILON);
        Ok(())
    }
}
