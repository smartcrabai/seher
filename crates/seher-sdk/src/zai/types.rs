use serde::Deserialize;

/// Top-level response from the z.ai quota API.
///
/// The structure is intentionally similar to [`crate::glm::types::GlmUsageResponse`]
/// but kept separate so that field-name drift between the two APIs does not leak
/// into shared code without an explicit test.
#[derive(Debug, Deserialize)]
pub struct ZaiUsageResponse {
    pub code: i32,
    pub msg: String,
    pub data: Option<ZaiQuotaData>,
    pub success: bool,
}

#[derive(Debug, Deserialize)]
pub struct ZaiQuotaData {
    pub limits: Vec<ZaiLimitRaw>,
}

impl ZaiQuotaData {
    /// Returns `true` when any single limit has been exhausted.
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.limits.iter().any(|l| l.percentage >= 100)
    }
}

#[derive(Debug, Deserialize)]
pub struct ZaiLimitRaw {
    #[serde(rename = "type")]
    pub limit_type: String,
    pub unit: i32,
    pub number: i32,
    pub usage: Option<i64>,
    pub remaining: Option<i64>,
    pub percentage: i32,
    #[serde(rename = "nextResetTime")]
    pub next_reset_time: Option<i64>,
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn test_deserialize_full_response() -> TestResult {
        let json = r#"{
            "code": 200,
            "msg": "ok",
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 40000000,
                        "remaining": 26371635,
                        "percentage": 34,
                        "nextResetTime": 1768507567547
                    }
                ]
            },
            "success": true
        }"#;

        let response: ZaiUsageResponse = serde_json::from_str(json)?;
        assert!(response.success);
        assert_eq!(response.code, 200);

        let data = response.data.unwrap();
        assert_eq!(data.limits.len(), 1);
        assert!(!data.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_percentage_100() -> TestResult {
        let json = r#"{
            "code": 200,
            "msg": "ok",
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 50000000,
                        "remaining": 0,
                        "percentage": 100,
                        "nextResetTime": null
                    }
                ]
            },
            "success": true
        }"#;

        let response: ZaiUsageResponse = serde_json::from_str(json)?;
        assert!(response.data.unwrap().is_limited());
        Ok(())
    }

    #[test]
    fn test_is_not_limited_when_all_below_100() -> TestResult {
        let json = r#"{
            "code": 200,
            "msg": "ok",
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 10000000,
                        "remaining": 40000000,
                        "percentage": 20,
                        "nextResetTime": 1768507567547
                    },
                    {
                        "type": "REQUESTS_LIMIT",
                        "unit": 1,
                        "number": 100,
                        "usage": 30,
                        "remaining": 70,
                        "percentage": 30,
                        "nextResetTime": null
                    }
                ]
            },
            "success": true
        }"#;

        let response: ZaiUsageResponse = serde_json::from_str(json)?;
        assert!(!response.data.unwrap().is_limited());
        Ok(())
    }

    #[test]
    fn test_error_response() -> TestResult {
        let json =
            r#"{"code": 1302, "msg": "rate limit exceeded", "data": null, "success": false}"#;
        let response: ZaiUsageResponse = serde_json::from_str(json)?;
        assert!(!response.success);
        assert_eq!(response.code, 1302);
        assert!(response.data.is_none());
        Ok(())
    }

    #[test]
    fn test_empty_limits_is_not_limited() -> TestResult {
        let json = r#"{"code": 200, "msg": "ok", "data": {"limits": []}, "success": true}"#;
        let response: ZaiUsageResponse = serde_json::from_str(json)?;
        let data = response.data.unwrap();
        assert!(!data.is_limited());
        Ok(())
    }

    #[test]
    fn test_next_reset_time_parsed_as_i64() -> TestResult {
        let json = r#"{
            "code": 200,
            "msg": "ok",
            "data": {
                "limits": [{
                    "type": "TOKENS_LIMIT",
                    "unit": 3,
                    "number": 5,
                    "usage": 100,
                    "remaining": 50,
                    "percentage": 50,
                    "nextResetTime": 1768507567547
                }]
            },
            "success": true
        }"#;

        let response: ZaiUsageResponse = serde_json::from_str(json)?;
        let limit = &response.data.unwrap().limits[0];
        assert_eq!(limit.next_reset_time, Some(1_768_507_567_547_i64));
        Ok(())
    }
}
