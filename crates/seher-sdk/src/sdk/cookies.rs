//! Browser cookie acquisition for limit checks.
//!
//! Replicates the cookie-selection flow from `cli.rs` (`get_cookies_for_domain` +
//! helpers) behind a single `BrowserSession` type usable by the resolve engine.

use std::future::Future;

use crate::browser::types::Profile;
use crate::{BrowserDetector, BrowserType, CodexClient, Cookie, CookieReader};

pub struct BrowserSession {
    detector: BrowserDetector,
    browsers: Vec<BrowserType>,
    browser_filter: Option<BrowserType>,
    profile_filter: Option<String>,
}

impl BrowserSession {
    #[must_use]
    pub fn detect(browser: Option<BrowserType>, profile: Option<String>) -> Self {
        let detector = BrowserDetector::new();
        let browsers = detector.detect_browsers();
        Self {
            detector,
            browsers,
            browser_filter: browser,
            profile_filter: profile,
        }
    }

    #[must_use]
    pub fn has_browsers(&self) -> bool {
        !self.browsers.is_empty()
    }

    /// Return cookies for `domain` from the first profile that contains a valid
    /// session cookie for it (codex's `chatgpt.com` is additionally validated by
    /// hitting the access-token endpoint).
    pub async fn cookies_for_domain(&self, domain: &str) -> Option<Vec<Cookie>> {
        let profiles = self.candidate_profiles();
        let candidates: Vec<Vec<Cookie>> = profiles
            .into_iter()
            .filter_map(|p| CookieReader::read_cookies(&p, domain).ok())
            .collect();
        select_cookie_candidate(domain, candidates, |cookies| async move {
            // Fail-closed: treat validator errors as "invalid" so a transient
            // ChatGPT API failure doesn't let an unverified cookie through.
            let is_valid = CodexClient::session_has_access_token(&cookies)
                .await
                .unwrap_or(false);
            (cookies, is_valid)
        })
        .await
    }

    fn candidate_profiles(&self) -> Vec<Profile> {
        candidate_profiles(
            &self.browsers,
            self.browser_filter,
            self.profile_filter.as_deref(),
            |bt, name| self.detector.get_profile(bt, Some(name)),
            |bt| self.detector.list_profiles(bt),
        )
    }
}

pub(crate) fn candidate_profiles<F, G>(
    browsers: &[BrowserType],
    browser_filter: Option<BrowserType>,
    profile_filter: Option<&str>,
    get_profile: F,
    list_profiles: G,
) -> Vec<Profile>
where
    F: Fn(BrowserType, &str) -> Option<Profile>,
    G: Fn(BrowserType) -> Vec<Profile>,
{
    let mut out: Vec<Profile> = Vec::new();
    for bt in browsers {
        if let Some(filter) = browser_filter
            && *bt != filter
        {
            continue;
        }
        if let Some(name) = profile_filter {
            if let Some(p) = get_profile(*bt, name) {
                out.push(p);
            }
        } else {
            out.extend(list_profiles(*bt));
        }
    }
    out
}

pub(crate) fn has_session_cookie(domain: &str, cookie: &Cookie) -> bool {
    match domain {
        "claude.ai" => cookie.name == "sessionKey",
        "chatgpt.com" => cookie.name.starts_with("__Secure-next-auth.session-token"),
        "github.com" => {
            cookie.name == "user_session" || cookie.name == "__Host-user_session_same_site"
        }
        _ => false,
    }
}

pub(crate) fn has_valid_session_cookie(domain: &str, cookie: &Cookie) -> bool {
    has_session_cookie(domain, cookie) && !cookie.is_expired()
}

pub(crate) async fn select_cookie_candidate<F, Fut>(
    domain: &str,
    candidates: Vec<Vec<Cookie>>,
    mut codex_validator: F,
) -> Option<Vec<Cookie>>
where
    F: FnMut(Vec<Cookie>) -> Fut,
    Fut: Future<Output = (Vec<Cookie>, bool)>,
{
    for cookies in candidates {
        if !cookies.iter().any(|c| has_valid_session_cookie(domain, c)) {
            continue;
        }
        if domain == "chatgpt.com" {
            let (cookies, is_valid) = codex_validator(cookies).await;
            if !is_valid {
                continue;
            }
            return Some(cookies);
        }
        return Some(cookies);
    }
    None
}

/// Provider→domain mapping for cookie-based limit checks. Returns `None` for
/// providers that do not require browser cookies (API-key-based providers).
#[must_use]
pub fn provider_to_domain(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("claude.ai"),
        "codex" => Some("chatgpt.com"),
        "copilot" => Some("github.com"),
        _ => None,
    }
}
