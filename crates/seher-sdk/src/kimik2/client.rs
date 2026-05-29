use std::time::Duration;

use super::types::KimiK2CreditsResponse;

const CREDITS_URL: &str = "https://kimi-k2.ai/api/user/credits";

pub struct KimiK2Client;

impl KimiK2Client {
    /// # Errors
    ///
    /// Returns an error if the API request fails or the response cannot be parsed.
    pub async fn fetch_credits(
        api_key: &str,
    ) -> Result<KimiK2CreditsResponse, Box<dyn std::error::Error>> {
        let client = Self::build_client()?;
        let response = client
            .get(CREDITS_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Kimi-K2 API error {status}: {body}").into());
        }

        let credits: KimiK2CreditsResponse = response.json().await?;
        Ok(credits)
    }

    fn build_client() -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
    }
}
