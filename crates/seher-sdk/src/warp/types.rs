use serde::Deserialize;

/// Top-level GraphQL response from the Warp API.
///
/// The Warp provider uses a GraphQL endpoint with a `GetRequestLimitInfo`
/// query that returns nested data under the `data` key.
#[derive(Debug, Deserialize)]
pub struct WarpLimitInfoResponse {
    pub data: WarpLimitData,
}

#[derive(Debug, Deserialize)]
pub struct WarpLimitData {
    #[serde(rename = "getRequestLimitInfo")]
    pub get_request_limit_info: WarpRequestLimitInfo,
}

/// Usage / limit information returned by the Warp GraphQL API.
#[derive(Debug, Deserialize)]
pub struct WarpRequestLimitInfo {
    /// Maximum number of requests allowed in the current window.
    pub limit: i64,
    /// Number of requests consumed so far.
    pub used: i64,
    /// Seconds until the window resets.  `None` if no reset is applicable.
    #[serde(rename = "resetInSeconds")]
    pub reset_in_seconds: Option<i64>,
}

impl WarpRequestLimitInfo {
    /// Returns `true` when the usage has reached or exceeded the limit.
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.used >= self.limit
    }

    /// Returns usage as a percentage (0–100). Returns 100.0 when limit is
    /// zero or negative.
    #[must_use]
    #[expect(clippy::cast_precision_loss)]
    pub fn utilization(&self) -> f64 {
        if self.limit > 0 {
            self.used as f64 / self.limit as f64 * 100.0
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
    fn test_deserialize_full_graphql_response() -> TestResult {
        let json = r#"{
            "data": {
                "getRequestLimitInfo": {
                    "limit": 50,
                    "used": 10,
                    "resetInSeconds": 3600
                }
            }
        }"#;

        let response: WarpLimitInfoResponse = serde_json::from_str(json)?;
        let info = &response.data.get_request_limit_info;
        assert_eq!(info.limit, 50);
        assert_eq!(info.used, 10);
        assert_eq!(info.reset_in_seconds, Some(3600));
        Ok(())
    }

    #[test]
    fn test_is_not_limited_when_below_limit() -> TestResult {
        let json = r#"{
            "data": {
                "getRequestLimitInfo": {
                    "limit": 50,
                    "used": 10,
                    "resetInSeconds": 3600
                }
            }
        }"#;

        let response: WarpLimitInfoResponse = serde_json::from_str(json)?;
        assert!(!response.data.get_request_limit_info.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_used_equals_limit() -> TestResult {
        let json = r#"{
            "data": {
                "getRequestLimitInfo": {
                    "limit": 50,
                    "used": 50,
                    "resetInSeconds": null
                }
            }
        }"#;

        let response: WarpLimitInfoResponse = serde_json::from_str(json)?;
        assert!(response.data.get_request_limit_info.is_limited());
        Ok(())
    }

    #[test]
    fn test_is_limited_when_used_exceeds_limit() -> TestResult {
        let json = r#"{
            "data": {
                "getRequestLimitInfo": {
                    "limit": 10,
                    "used": 15,
                    "resetInSeconds": null
                }
            }
        }"#;

        let response: WarpLimitInfoResponse = serde_json::from_str(json)?;
        assert!(response.data.get_request_limit_info.is_limited());
        Ok(())
    }

    #[test]
    fn test_utilization_computed_correctly() {
        let info = WarpRequestLimitInfo {
            limit: 200,
            used: 50,
            reset_in_seconds: None,
        };
        assert!((info.utilization() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_utilization_returns_100_when_zero_limit() {
        let info = WarpRequestLimitInfo {
            limit: 0,
            used: 0,
            reset_in_seconds: None,
        };
        assert!((info.utilization() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_reset_in_seconds_is_optional() -> TestResult {
        let json = r#"{
            "data": {
                "getRequestLimitInfo": {
                    "limit": 100,
                    "used": 0
                }
            }
        }"#;

        let response: WarpLimitInfoResponse = serde_json::from_str(json)?;
        assert!(
            response
                .data
                .get_request_limit_info
                .reset_in_seconds
                .is_none()
        );
        Ok(())
    }
}
