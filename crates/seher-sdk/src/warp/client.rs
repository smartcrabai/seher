use std::time::Duration;

use super::types::WarpLimitInfoResponse;

const GRAPHQL_URL: &str = "https://api.warp.dev/graphql";

const QUERY: &str = r#"{ "query": "{ getRequestLimitInfo { limit used resetInSeconds } }" }"#;

pub struct WarpClient;

impl WarpClient {
    /// # Errors
    ///
    /// Returns an error if the GraphQL request fails or the response cannot be parsed.
    pub async fn fetch_limit_info(
        api_key: &str,
    ) -> Result<WarpLimitInfoResponse, Box<dyn std::error::Error>> {
        let client = Self::build_client()?;
        let response = client
            .post(GRAPHQL_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .body(QUERY)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Warp API error {status}: {body}").into());
        }

        let info: WarpLimitInfoResponse = response.json().await?;
        Ok(info)
    }

    fn build_client() -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
    }
}
