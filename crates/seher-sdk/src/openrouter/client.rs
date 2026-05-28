use std::time::Duration;

use super::types::CreditsResponse;

const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

pub struct OpenRouterClient;

impl OpenRouterClient {
    /// # Errors
    ///
    /// Returns an error if the API request fails or the response cannot be parsed.
    pub async fn fetch_credits(
        management_key: &str,
    ) -> Result<CreditsResponse, Box<dyn std::error::Error>> {
        let client = Self::build_client()?;
        let response = client
            .get(CREDITS_URL)
            .header("Authorization", format!("Bearer {management_key}"))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("OpenRouter API error {status}: {body}").into());
        }

        let credits: CreditsResponse = response.json().await?;
        Ok(credits)
    }

    fn build_client() -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
    }
}
