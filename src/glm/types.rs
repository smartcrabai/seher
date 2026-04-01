use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GlmUsageResponse {
    pub code: i32,
    pub msg: String,
    pub data: Option<GlmQuotaData>,
    pub success: bool,
}

#[derive(Debug, Deserialize)]
pub struct GlmQuotaData {
    pub limits: Vec<GlmLimitRaw>,
    #[serde(
        alias = "planName",
        alias = "plan",
        alias = "plan_type",
        alias = "packageName"
    )]
    pub plan_name: Option<String>,
}

impl GlmQuotaData {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.limits.iter().any(|l| l.percentage >= 100)
    }
}

#[derive(Debug, Deserialize)]
pub struct GlmLimitRaw {
    #[serde(rename = "type")]
    pub limit_type: String,
    pub unit: i32,
    pub number: i32,
    pub usage: Option<i64>,
    #[serde(rename = "currentValue")]
    pub current_value: Option<i64>,
    pub remaining: Option<i64>,
    pub percentage: i32,
    #[serde(default, rename = "usageDetails")]
    pub usage_details: Vec<GlmUsageDetail>,
    #[serde(rename = "nextResetTime")]
    pub next_reset_time: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct GlmUsageDetail {
    #[serde(rename = "modelCode")]
    pub model_code: Option<String>,
    pub usage: Option<i64>,
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
            "msg": "Operation successful",
            "data": {
                "limits": [
                    {
                        "type": "TIME_LIMIT",
                        "unit": 5,
                        "number": 1,
                        "usage": 100,
                        "currentValue": 102,
                        "remaining": 0,
                        "percentage": 100,
                        "usageDetails": [{"modelCode": "search-prime", "usage": 95}]
                    },
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 40000000,
                        "currentValue": 13628365,
                        "remaining": 26371635,
                        "percentage": 34,
                        "nextResetTime": 1768507567547
                    }
                ],
                "planName": "Pro"
            },
            "success": true
        }"#;

        let response: GlmUsageResponse = serde_json::from_str(json)?;
        assert!(response.success);
        assert_eq!(response.code, 200);

        let data = response.data.unwrap();
        assert_eq!(data.plan_name.as_deref(), Some("Pro"));
        assert_eq!(data.limits.len(), 2);
        assert!(data.is_limited()); // TIME_LIMIT has percentage=100
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
                        "usage": 100,
                        "currentValue": 100,
                        "remaining": 0,
                        "percentage": 100,
                        "usageDetails": []
                    }
                ]
            },
            "success": true
        }"#;

        let response: GlmUsageResponse = serde_json::from_str(json)?;
        assert!(response.data.unwrap().is_limited());
        Ok(())
    }

    #[test]
    fn test_is_not_limited_when_below_100() -> TestResult {
        let json = r#"{
            "code": 200,
            "msg": "ok",
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 100,
                        "currentValue": 50,
                        "remaining": 50,
                        "percentage": 50,
                        "usageDetails": []
                    }
                ]
            },
            "success": true
        }"#;

        let response: GlmUsageResponse = serde_json::from_str(json)?;
        assert!(!response.data.unwrap().is_limited());
        Ok(())
    }

    #[test]
    fn test_plan_name_aliases() -> TestResult {
        for alias in &["planName", "plan", "plan_type", "packageName"] {
            let json = format!(
                r#"{{"code": 200, "msg": "ok", "data": {{"limits": [], "{alias}": "Lite"}}, "success": true}}"#
            );
            let response: GlmUsageResponse = serde_json::from_str(&json)?;
            assert_eq!(
                response.data.unwrap().plan_name.as_deref(),
                Some("Lite"),
                "Failed for alias: {alias}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_error_response() -> TestResult {
        let json =
            r#"{"code": 1302, "msg": "rate limit exceeded", "data": null, "success": false}"#;
        let response: GlmUsageResponse = serde_json::from_str(json)?;
        assert!(!response.success);
        assert_eq!(response.code, 1302);
        assert!(response.data.is_none());
        Ok(())
    }

    #[test]
    fn test_empty_limits() -> TestResult {
        let json = r#"{"code": 200, "msg": "ok", "data": {"limits": []}, "success": true}"#;
        let response: GlmUsageResponse = serde_json::from_str(json)?;
        let data = response.data.unwrap();
        assert!(!data.is_limited());
        Ok(())
    }

    #[test]
    fn test_usage_details_deserialize() -> TestResult {
        let json = r#"{
            "code": 200,
            "msg": "ok",
            "data": {
                "limits": [{
                    "type": "TIME_LIMIT",
                    "unit": 5,
                    "number": 1,
                    "usage": 100,
                    "currentValue": 50,
                    "remaining": 50,
                    "percentage": 50,
                    "usageDetails": [
                        {"modelCode": "chatglm-pro", "usage": 30},
                        {"modelCode": "search-prime", "usage": 20}
                    ]
                }]
            },
            "success": true
        }"#;

        let response: GlmUsageResponse = serde_json::from_str(json)?;
        let limit = &response.data.unwrap().limits[0];
        assert_eq!(limit.usage_details.len(), 2);
        assert_eq!(
            limit.usage_details[0].model_code.as_deref(),
            Some("chatglm-pro")
        );
        assert_eq!(limit.usage_details[0].usage, Some(30));
        Ok(())
    }
}
