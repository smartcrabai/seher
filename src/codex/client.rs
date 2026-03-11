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
    pub async fn fetch_usage(
        cookies: &[Cookie],
    ) -> Result<CodexUsageResponse, Box<dyn std::error::Error>> {
        let cookie_header = Self::build_cookie_header(cookies);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build()?;

        let access_token = Self::fetch_access_token(&client, &cookie_header).await?;

        let response = client
            .get(USAGE_URL)
            .header("Cookie", &cookie_header)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Referer", USAGE_REFERER)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let body = Self::truncate_body(&body);
            return Err(format!("Codex usage API error: {} - {}", status, body).into());
        }

        Ok(response.json().await?)
    }

    async fn fetch_access_token(
        client: &reqwest::Client,
        cookie_header: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
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
            return Err(format!("Codex session API error: {} - {}", status, body).into());
        }

        let session: SessionResponse = response.json().await?;
        match session.access_token {
            Some(token) if !token.is_empty() => Ok(token),
            _ => {
                let detail = session
                    .error
                    .unwrap_or_else(|| "missing access token".to_string());
                Err(format!("Codex session did not return an access token: {}", detail).into())
            }
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
            format!("{}...", preview)
        } else {
            preview
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CodexClient;

    #[test]
    fn truncate_body_preserves_utf8_boundaries() {
        let body = "あ".repeat(201);

        let truncated = CodexClient::truncate_body(&body);

        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.chars().count(), 203);
        assert!(truncated.starts_with(&"あ".repeat(200)));
    }
}
