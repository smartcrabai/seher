use std::time::Duration;

use super::types::ZaiUsageResponse;

const DEFAULT_QUOTA_URL: &str = "https://api.z.ai/api/paas/quota/limit";

pub struct ZaiClient;

impl ZaiClient {
    /// # Errors
    ///
    /// Returns an error if the API request fails or the response cannot be parsed.
    pub async fn fetch_quota(
        api_key: &str,
        quota_url: Option<&str>,
    ) -> Result<ZaiUsageResponse, Box<dyn std::error::Error>> {
        let url = quota_url.unwrap_or(DEFAULT_QUOTA_URL);
        let client = Self::build_client()?;
        let response = client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Z.AI API error {status}: {body}").into());
        }

        let quota: ZaiUsageResponse = response.json().await?;
        Ok(quota)
    }

    fn build_client() -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
    }
}
