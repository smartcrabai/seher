use super::error::{ClaudeApiError, Result};
use super::types::UsageResponse;
use crate::Cookie;

fn urldecode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(|c| (c as char).to_digit(16));
            let lo = chars.next().and_then(|c| (c as char).to_digit(16));
            if let (Some(h), Some(l)) = (hi, lo)
                && let Ok(byte) = u8::try_from(h * 16 + l)
            {
                result.push(byte as char);
            }
        } else {
            result.push(b as char);
        }
    }
    result
}

fn extract_uuid(s: &str) -> Option<String> {
    // Find a UUID pattern (8-4-4-4-12 hex digits)
    let bytes = s.as_bytes();
    let hex = |b: u8| b.is_ascii_hexdigit();
    for i in 0..bytes.len() {
        if i + 36 > bytes.len() {
            break;
        }
        let candidate = &bytes[i..i + 36];
        if candidate[8] == b'-'
            && candidate[13] == b'-'
            && candidate[18] == b'-'
            && candidate[23] == b'-'
            && candidate[..8].iter().all(|b| hex(*b))
            && candidate[9..13].iter().all(|b| hex(*b))
            && candidate[14..18].iter().all(|b| hex(*b))
            && candidate[19..23].iter().all(|b| hex(*b))
            && candidate[24..36].iter().all(|b| hex(*b))
        {
            return Some(String::from_utf8_lossy(candidate).to_string());
        }
    }
    None
}

pub struct ClaudeClient;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";

impl ClaudeClient {
    /// # Errors
    ///
    /// Returns an error if the org ID cookie is missing, the API request fails, or the response
    /// cannot be parsed.
    pub async fn fetch_usage(cookies: &[Cookie]) -> Result<UsageResponse> {
        let org_id = Self::find_org_id(cookies)?;
        let cookie_header = Self::build_cookie_header(cookies);

        let url = format!("https://claude.ai/api/organizations/{org_id}/usage");

        let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;

        let response = client
            .get(&url)
            .header("Cookie", &cookie_header)
            .header("Accept", "application/json")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Referer", "https://claude.ai/")
            .header("Origin", "https://claude.ai")
            .header("DNT", "1")
            .header("sec-ch-ua-platform", "\"macOS\"")
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-origin")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            // Truncate Cloudflare HTML for readability
            let body = if body.len() > 200 {
                format!("{}...", &body[..200])
            } else {
                body
            };
            return Err(ClaudeApiError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let usage: UsageResponse = response.json().await?;
        Ok(usage)
    }

    fn find_org_id(cookies: &[Cookie]) -> Result<String> {
        let raw = cookies
            .iter()
            .find(|c| c.name == "lastActiveOrg")
            .map(|c| c.value.clone())
            .ok_or_else(|| {
                ClaudeApiError::CookieNotFound("lastActiveOrg cookie not found".to_string())
            })?;

        // URL-decode and extract UUID pattern
        let decoded = urldecode(&raw);
        extract_uuid(&decoded).ok_or_else(|| {
            ClaudeApiError::CookieNotFound(format!(
                "lastActiveOrg does not contain a valid UUID: {decoded}"
            ))
        })
    }

    fn build_cookie_header(cookies: &[Cookie]) -> String {
        cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }
}
