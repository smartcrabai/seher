//! Authoritative OpenCode Go (zen/go) usage via the `opencode.ai` web dashboard.
//!
//! Ported from CodexBar's `OpenCodeGoUsageFetcher`
//! (steipete/codexbar): the local SQLite DB only records *this machine's*
//! spend, so it badly undercounts an account whose real usage spans multiple
//! machines/sessions. The hosted dashboard is the source of truth.
//!
//! Flow:
//!  1. Resolve the workspace id via the `_server` RPC (`X-Server-Id` =
//!     [`WORKSPACES_SERVER_ID`]), scraping `id:"wrk_..."`.
//!  2. GET `https://opencode.ai/workspace/{id}/go` and scrape the
//!     `rollingUsage` / `weeklyUsage` / `monthlyUsage` `usagePercent` +
//!     `resetInSec` fields.
//!
//! Auth is the `opencode.ai` browser session cookie (`auth` / `__Host-auth`).

use chrono::{DateTime, Duration, Utc};

use crate::Cookie;

const BASE_URL: &str = "https://opencode.ai";
const SERVER_URL: &str = "https://opencode.ai/_server";
const WORKSPACES_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Debug, thiserror::Error)]
pub enum OpencodeGoRemoteError {
    #[error("opencode.ai session cookie is invalid or expired")]
    InvalidCredentials,
    #[error("could not determine OpenCode workspace id")]
    MissingWorkspace,
    #[error("OpenCode Go usage page is missing usage fields")]
    MissingUsage,
    #[error("HTTP request to opencode.ai failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// A single usage window scraped from the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteWindow {
    /// `rolling` (5h), `weekly`, or `monthly`.
    pub name: &'static str,
    pub used_percent: f64,
    pub reset_in_sec: Option<i64>,
}

impl RemoteWindow {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.used_percent >= 100.0
    }

    #[must_use]
    pub fn resets_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.reset_in_sec
            .filter(|s| *s > 0)
            .map(|s| now + Duration::seconds(s))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteUsage {
    pub windows: Vec<RemoteWindow>,
}

impl RemoteUsage {
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.windows.iter().any(RemoteWindow::is_limited)
    }

    /// Latest reset among the currently-limited windows.
    #[must_use]
    pub fn reset_time(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.windows
            .iter()
            .filter(|w| w.is_limited())
            .filter_map(|w| w.resets_at(now))
            .max()
    }
}

fn build_cookie_header(cookies: &[Cookie]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

fn looks_signed_out(text: &str) -> bool {
    let lower = text.to_lowercase();
    // The dashboard redirects to a sign-in page when the session is invalid.
    (lower.contains("sign in") || lower.contains("log in") || lower.contains("login"))
        && !lower.contains("usagepercent")
        && !lower.contains("rollingusage")
}

/// Extract the first `wrk_...` id from the `_server` response.
fn parse_workspace_id(text: &str) -> Option<String> {
    // Primary: `id:"wrk_..."` (matches CodexBar's `id\s*:\s*"(wrk_[^"]+)"`).
    if let Some(id) = regex::Regex::new(r#"id\s*:\s*"(wrk_[^"]+)""#)
        .ok()
        .and_then(|re| {
            re.captures(text)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
        })
    {
        return Some(id);
    }
    // Fallback: any bare `wrk_<alnum>` token.
    regex::Regex::new(r"wrk_[A-Za-z0-9]+")
        .ok()
        .and_then(|re| re.find(text).map(|m| m.as_str().to_string()))
}

fn extract_window(text: &str, key: &str) -> Option<RemoteWindow> {
    let name: &'static str = match key {
        "rollingUsage" => "rolling",
        "weeklyUsage" => "weekly",
        "monthlyUsage" => "monthly",
        _ => return None,
    };
    let percent = extract_number(text, key, "usagePercent")?;
    let reset = extract_number(text, key, "resetInSec").map(|v| v as i64);
    Some(RemoteWindow {
        name,
        used_percent: percent,
        reset_in_sec: reset,
    })
}

/// Find `<block_key> ... <field>: <number>` within the same `{...}` object,
/// mirroring CodexBar's `<block>[^}]*?<field>\s*:\s*([0-9.]+)`.
fn extract_number(text: &str, block_key: &str, field: &str) -> Option<f64> {
    let pattern = format!(r"{block_key}[^}}]*?{field}\s*:\s*([0-9]+(?:\.[0-9]+)?)");
    let re = regex::Regex::new(&pattern).ok()?;
    re.captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
}

/// Parse the dashboard payload into usage windows. Public for unit testing.
///
/// # Errors
///
/// Returns [`OpencodeGoRemoteError::MissingUsage`] when neither the rolling nor
/// weekly window can be located.
pub fn parse_usage(text: &str) -> Result<RemoteUsage, OpencodeGoRemoteError> {
    let mut windows = Vec::new();
    for key in ["rollingUsage", "weeklyUsage", "monthlyUsage"] {
        if let Some(w) = extract_window(text, key) {
            windows.push(w);
        }
    }
    if windows.is_empty() {
        return Err(OpencodeGoRemoteError::MissingUsage);
    }
    Ok(RemoteUsage { windows })
}

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        // Match CodexBar's redirect guard: don't follow cross-host redirects to
        // a sign-in page; we detect sign-out from the body instead.
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

async fn fetch_workspace_id(
    client: &reqwest::Client,
    cookie_header: &str,
) -> Result<String, OpencodeGoRemoteError> {
    let do_get = |method_post: bool| async move {
        let url = format!("{SERVER_URL}?id={WORKSPACES_SERVER_ID}");
        let mut req = if method_post {
            client
                .post(&url)
                .body("[]")
                .header("Content-Type", "application/json")
        } else {
            client.get(&url)
        };
        req = req
            .header("Cookie", cookie_header)
            .header("X-Server-Id", WORKSPACES_SERVER_ID)
            .header("X-Server-Instance", "server-fn:seher")
            .header("Origin", BASE_URL)
            .header("Referer", BASE_URL)
            .header(
                "Accept",
                "text/javascript, application/json;q=0.9, */*;q=0.8",
            );
        req.send().await?.text().await
    };

    let text = do_get(false).await?;
    if looks_signed_out(&text) {
        return Err(OpencodeGoRemoteError::InvalidCredentials);
    }
    if let Some(id) = parse_workspace_id(&text) {
        return Ok(id);
    }
    // Retry with POST (CodexBar fallback).
    let text = do_get(true).await?;
    if looks_signed_out(&text) {
        return Err(OpencodeGoRemoteError::InvalidCredentials);
    }
    parse_workspace_id(&text).ok_or(OpencodeGoRemoteError::MissingWorkspace)
}

async fn fetch_usage_page(
    client: &reqwest::Client,
    cookie_header: &str,
    workspace_id: &str,
) -> Result<String, OpencodeGoRemoteError> {
    let url = format!("{BASE_URL}/workspace/{workspace_id}/go");
    let text = client
        .get(&url)
        .header("Cookie", cookie_header)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
        )
        .header("Referer", BASE_URL)
        .send()
        .await?
        .text()
        .await?;
    if looks_signed_out(&text) {
        return Err(OpencodeGoRemoteError::InvalidCredentials);
    }
    Ok(text)
}

/// Fetch authoritative OpenCode Go usage from the hosted dashboard using the
/// given `opencode.ai` session cookies.
///
/// # Errors
///
/// Returns [`OpencodeGoRemoteError`] on auth failure, missing workspace, missing
/// usage fields, or transport error.
pub async fn fetch_usage(cookies: &[Cookie]) -> Result<RemoteUsage, OpencodeGoRemoteError> {
    let cookie_header = build_cookie_header(cookies);
    let client = http_client()?;
    let workspace_id = fetch_workspace_id(&client, &cookie_header).await?;
    let text = fetch_usage_page(&client, &cookie_header, &workspace_id).await?;
    parse_usage(&text)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspace_id_from_server_payload() {
        let text = r#"...,{id:"wrk_abc123DEF",name:"My WS"},..."#;
        assert_eq!(parse_workspace_id(text).as_deref(), Some("wrk_abc123DEF"));
    }

    #[test]
    fn parses_workspace_id_bare_fallback() {
        let text = r#"{"workspaces":["wrk_xyz789"]}"#;
        assert_eq!(parse_workspace_id(text).as_deref(), Some("wrk_xyz789"));
    }

    #[test]
    fn parses_rolling_weekly_monthly_usage() {
        let text = r#"
          rollingUsage: { usagePercent: 100, resetInSec: 3600 },
          weeklyUsage: { usagePercent: 42.5, resetInSec: 86400 },
          monthlyUsage: { usagePercent: 7, resetInSec: 1000000 }
        "#;
        let usage = parse_usage(text).expect("parse");
        assert_eq!(usage.windows.len(), 3);
        let rolling = usage.windows.iter().find(|w| w.name == "rolling").unwrap();
        assert!((rolling.used_percent - 100.0).abs() < 1e-9);
        assert_eq!(rolling.reset_in_sec, Some(3600));
        assert!(rolling.is_limited());
        assert!(usage.is_limited());
    }

    #[test]
    fn not_limited_when_all_below_100() {
        let text = r#"rollingUsage: { usagePercent: 20.5, resetInSec: 100 } weeklyUsage: { usagePercent: 80, resetInSec: 200 }"#;
        let usage = parse_usage(text).expect("parse");
        assert!(!usage.is_limited());
    }

    #[test]
    fn missing_usage_fields_errors() {
        let err = parse_usage("no usage here").expect_err("should fail");
        assert!(matches!(err, OpencodeGoRemoteError::MissingUsage));
    }

    #[test]
    fn reset_time_is_latest_limited_window() {
        let now = Utc::now();
        let usage = RemoteUsage {
            windows: vec![
                RemoteWindow {
                    name: "rolling",
                    used_percent: 100.0,
                    reset_in_sec: Some(3600),
                },
                RemoteWindow {
                    name: "weekly",
                    used_percent: 100.0,
                    reset_in_sec: Some(7200),
                },
                RemoteWindow {
                    name: "monthly",
                    used_percent: 10.0,
                    reset_in_sec: Some(999_999),
                },
            ],
        };
        let reset = usage.reset_time(now).expect("reset");
        // max of the two limited windows (7200s), monthly is not limited so ignored.
        assert!((reset - now).num_seconds() >= 7000);
        assert!((reset - now).num_seconds() <= 7300);
    }

    #[test]
    fn signed_out_detection() {
        assert!(looks_signed_out(
            "<html><body>Please sign in to continue</body></html>"
        ));
        assert!(!looks_signed_out("rollingUsage: { usagePercent: 50 }"));
    }
}
