use super::types::CreditsResponse;

pub struct OpenRouterClient;

impl OpenRouterClient {
    /// # Errors
    ///
    /// Always returns an error as this function is not yet implemented.
    pub fn fetch_credits(
        _management_key: &str,
    ) -> Result<CreditsResponse, Box<dyn std::error::Error>> {
        Err("OpenRouter Credits API client not yet implemented".into())
    }
}
