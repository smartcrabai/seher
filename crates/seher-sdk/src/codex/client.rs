use super::types::CodexUsageResponse;
use crate::Cookie;
use serde::Deserialize;
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
const SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const USAGE_REFERER: &str = "https://chatgpt.com/codex/settings/usage";

#[derive(Debug, Deserialize)]
struct SessionResponse {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    error: Option<String>,
}

pub struct CodexClient;

impl CodexClient {
    /// # Errors
    ///
    /// Returns an error if fetching the session or usage API fails, or the response cannot be
    /// parsed.
    pub async fn fetch_usage(
        cookies: &[Cookie],
    ) -> Result<CodexUsageResponse, Box<dyn std::error::Error>> {
        let cookie_header = Self::build_cookie_header(cookies);
        let client = Self::build_client()?;

        let access_token = Self::fetch_access_token(&client, &cookie_header).await?;

        let response = client
            .get(USAGE_URL)
            .header("Cookie", &cookie_header)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Referer", USAGE_REFERER)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let body = Self::truncate_body(&body);
            return Err(format!("Codex usage API error: {status} - {body}").into());
        }

        Ok(response.json().await?)
    }

    /// # Errors
    ///
    /// Returns an error if the session API request fails or the response cannot be parsed.
    pub async fn session_has_access_token(
        cookies: &[Cookie],
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cookie_header = Self::build_cookie_header(cookies);
        let client = Self::build_client()?;
        let session = Self::fetch_session(&client, &cookie_header).await?;

        Ok(Self::extract_access_token(session).is_ok())
    }

    fn build_client() -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build()
    }

    async fn fetch_access_token(
        client: &reqwest::Client,
        cookie_header: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = Self::fetch_session(client, cookie_header).await?;

        Self::extract_access_token(session).map_err(|detail| {
            format!("Codex session did not return an access token: {detail}").into()
        })
    }

    async fn fetch_session(
        client: &reqwest::Client,
        cookie_header: &str,
    ) -> Result<SessionResponse, Box<dyn std::error::Error>> {
        let response = client
            .get(SESSION_URL)
            .header("Cookie", cookie_header)
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let body = Self::truncate_body(&body);
            return Err(format!("Codex session API error: {status} - {body}").into());
        }

        Ok(response.json().await?)
    }

    fn extract_access_token(session: SessionResponse) -> Result<String, String> {
        match session.access_token {
            Some(token) if !token.is_empty() => Ok(token),
            _ => Err(session
                .error
                .unwrap_or_else(|| "missing access token".to_string())),
        }
    }

    fn build_cookie_header(cookies: &[Cookie]) -> String {
        cookies
            .iter()
            .filter(|c| !c.value.bytes().any(|b| b < 0x20 || b == 0x7f))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn truncate_body(body: &str) -> String {
        let mut chars = body.chars();
        let preview: String = chars.by_ref().take(200).collect();
        if chars.next().is_some() {
            format!("{preview}...")
        } else {
            preview
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CodexClient, SessionResponse};

    #[test]
    fn extract_access_token_returns_token_when_present() {
        let session = SessionResponse {
            access_token: Some("token-123".to_string()),
            error: Some("ignored".to_string()),
        };

        let result = CodexClient::extract_access_token(session);

        assert!(result.is_ok());
        assert_eq!(result.ok().as_deref(), Some("token-123"));
    }

    #[test]
    fn extract_access_token_returns_missing_when_absent() {
        let session = SessionResponse {
            access_token: None,
            error: None,
        };

        let result = CodexClient::extract_access_token(session);

        assert!(result.is_err());
        assert_eq!(result.err().as_deref(), Some("missing access token"));
    }

    #[test]
    fn extract_access_token_prefers_server_error_when_present() {
        let session = SessionResponse {
            access_token: None,
            error: Some("session expired".to_string()),
        };

        let result = CodexClient::extract_access_token(session);

        assert!(result.is_err());
        assert_eq!(result.err().as_deref(), Some("session expired"));
    }

    #[test]
    fn truncate_body_preserves_utf8_boundaries() {
        let body = "a".repeat(201);

        let truncated = CodexClient::truncate_body(&body);

        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.chars().count(), 203);
        assert!(truncated.starts_with(&"a".repeat(200)));
    }
}
