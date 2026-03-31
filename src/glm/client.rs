use std::time::Duration;

use super::types::GlmUsageResponse;

const QUOTA_URL: &str = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";

pub struct GlmClient;

impl GlmClient {
    /// # Errors
    ///
    /// Returns an error if the API request fails or the response cannot be parsed.
    pub async fn fetch_quota(
        api_key: &str,
    ) -> Result<GlmUsageResponse, Box<dyn std::error::Error>> {
        let client = Self::build_client()?;
        let response = client
            .get(QUOTA_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("GLM API error {status}: {body}").into());
        }

        let quota: GlmUsageResponse = response.json().await?;
        Ok(quota)
    }

    fn build_client() -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
    }
}
