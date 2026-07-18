//! Maps a codexbar usage payload to an [`AgentLimit`].
//!
//! Mirrors `seher-ts/packages/sdk/src/codexbar/limit.ts`: every rate window
//! (primary/secondary/tertiary + extra windows) at `usedPercent >= 100` counts
//! as limited, and the earliest reset is returned so the agent waits the minimum
//! amount of time. Extension over seher-ts: a window whose `resetsAt` has
//! already passed is treated as a stale snapshot (the window has presumably
//! already reset server-side) rather than as evidence of an active limit, so
//! it is excluded from consideration.

use chrono::{DateTime, Utc};

use super::client::{RunCodexBarUsageOptions, run_codexbar_usage};
use super::errors::CodexBarError;
use super::types::{CodexBarUsageResponse, CodexBarWindow};

/// Outcome of a rate-limit check for a single provider.
#[derive(Debug, Clone)]
pub enum AgentLimit {
    /// The provider has quota available.
    NotLimited,
    /// The provider is at-limit; `reset_time` is the earliest moment it frees up
    /// (when known).
    Limited { reset_time: Option<DateTime<Utc>> },
}

/// Reset fallback when codexbar reports a limited window without a parseable
/// `resetsAt` (matches seher-ts's 5-minute fallback).
const FALLBACK_RESET_SECS: i64 = 5 * 60;

fn parse_resets_at(resets_at: Option<&str>, now: DateTime<Utc>) -> DateTime<Utc> {
    if let Some(s) = resets_at
        && let Ok(parsed) = DateTime::parse_from_rfc3339(s)
    {
        return parsed.with_timezone(&Utc);
    }
    now + chrono::Duration::seconds(FALLBACK_RESET_SECS)
}

fn is_limited(window: &CodexBarWindow) -> bool {
    window.used_percent >= 100.0
}

fn classify(response: &CodexBarUsageResponse, now: DateTime<Utc>) -> AgentLimit {
    let usage = &response.usage;
    let mut windows: Vec<&CodexBarWindow> = Vec::new();
    windows.extend(usage.primary.as_ref());
    windows.extend(usage.secondary.as_ref());
    windows.extend(usage.tertiary.as_ref());
    if let Some(extra) = &usage.extra_rate_windows {
        windows.extend(extra.iter().map(|named| &named.window));
    }

    let earliest = windows
        .into_iter()
        .filter(|w| is_limited(w))
        .map(|w| parse_resets_at(w.resets_at.as_deref(), now))
        // A window whose resetsAt has already passed is a stale snapshot (it
        // has presumably reset server-side already), not an active limit.
        // Windows with no parseable resetsAt fall back to `now + 5m` (see
        // `parse_resets_at`), which is always in the future, so this filter
        // never drops the no-resetsAt fallback case.
        .filter(|reset| *reset > now)
        .min();

    match earliest {
        Some(reset_time) => AgentLimit::Limited {
            reset_time: Some(reset_time),
        },
        None => AgentLimit::NotLimited,
    }
}

/// Determine whether `provider` is rate-limited by invoking codexbar with default options.
///
/// # Errors
///
/// Propagates [`CodexBarError`] from [`run_codexbar_usage`].
pub async fn check_limit(provider: &str) -> Result<AgentLimit, CodexBarError> {
    check_limit_with(provider, &RunCodexBarUsageOptions::default()).await
}

/// Like [`check_limit`] but with explicit [`RunCodexBarUsageOptions`].
///
/// # Errors
///
/// Propagates [`CodexBarError`] from [`run_codexbar_usage`].
pub async fn check_limit_with(
    provider: &str,
    opts: &RunCodexBarUsageOptions,
) -> Result<AgentLimit, CodexBarError> {
    let response = run_codexbar_usage(provider, opts).await?;
    Ok(classify(&response, Utc::now()))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    fn window(used_percent: f64, resets_at: Option<&str>) -> CodexBarWindow {
        CodexBarWindow {
            used_percent,
            window_minutes: None,
            resets_at: resets_at.map(ToString::to_string),
            reset_description: None,
            next_regen_percent: None,
        }
    }

    fn response(primary: CodexBarWindow, secondary: CodexBarWindow) -> CodexBarUsageResponse {
        CodexBarUsageResponse {
            provider: "codex".to_string(),
            usage: super::super::types::CodexBarUsage {
                primary: Some(primary),
                secondary: Some(secondary),
                tertiary: None,
                extra_rate_windows: None,
            },
        }
    }

    #[test]
    fn not_limited_when_all_windows_under_100() {
        let resp = response(window(50.0, None), window(30.0, None));
        assert!(matches!(
            classify(&resp, Utc::now()),
            AgentLimit::NotLimited
        ));
    }

    #[test]
    fn limited_when_any_window_at_100() {
        let resp = response(
            window(100.0, Some("2099-01-01T00:00:00Z")),
            window(30.0, None),
        );
        match classify(&resp, Utc::now()) {
            AgentLimit::Limited { reset_time } => {
                let reset = reset_time.expect("reset present");
                assert_eq!(
                    reset,
                    DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
                        .expect("parse")
                        .with_timezone(&Utc)
                );
            }
            AgentLimit::NotLimited => panic!("expected limited"),
        }
    }

    #[test]
    fn picks_earliest_reset_across_limited_windows() {
        let resp = response(
            window(100.0, Some("2099-06-01T00:00:00Z")),
            window(100.0, Some("2099-01-01T00:00:00Z")),
        );
        match classify(&resp, Utc::now()) {
            AgentLimit::Limited { reset_time } => {
                let reset = reset_time.expect("reset present");
                assert_eq!(
                    reset,
                    DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
                        .expect("parse")
                        .with_timezone(&Utc)
                );
            }
            AgentLimit::NotLimited => panic!("expected limited"),
        }
    }

    #[test]
    fn limited_window_without_resets_at_uses_fallback() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("parse")
            .with_timezone(&Utc);
        let resp = response(window(100.0, None), window(10.0, None));
        match classify(&resp, now) {
            AgentLimit::Limited { reset_time } => {
                let reset = reset_time.expect("reset present");
                assert_eq!(reset, now + chrono::Duration::seconds(FALLBACK_RESET_SECS));
            }
            AgentLimit::NotLimited => panic!("expected limited"),
        }
    }

    #[test]
    fn not_limited_when_resets_at_already_passed() {
        // A window at 100% whose resetsAt is in the past is a stale snapshot
        // (it has presumably already reset server-side), not an active limit.
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("parse")
            .with_timezone(&Utc);
        let resp = response(
            window(100.0, Some("2025-01-01T00:00:00Z")),
            window(10.0, None),
        );
        assert!(matches!(classify(&resp, now), AgentLimit::NotLimited));
    }

    #[test]
    fn ignores_stale_reset_but_limits_on_future_reset() {
        // One window is 100% with a past resetsAt (stale, ignored) and the
        // other is 100% with a future resetsAt (still active) -- the result
        // should be Limited, using the future window's reset time.
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("parse")
            .with_timezone(&Utc);
        let resp = response(
            window(100.0, Some("2025-01-01T00:00:00Z")),
            window(100.0, Some("2099-01-01T00:00:00Z")),
        );
        match classify(&resp, now) {
            AgentLimit::Limited { reset_time } => {
                let reset = reset_time.expect("reset present");
                assert_eq!(
                    reset,
                    DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
                        .expect("parse")
                        .with_timezone(&Utc)
                );
            }
            AgentLimit::NotLimited => panic!("expected limited"),
        }
    }

    #[test]
    fn counts_extra_rate_windows() {
        let mut resp = response(window(10.0, None), window(20.0, None));
        resp.usage.extra_rate_windows = Some(vec![super::super::types::NamedCodexBarWindow {
            id: "daily".to_string(),
            title: "Daily".to_string(),
            window: window(100.0, Some("2099-03-03T00:00:00Z")),
        }]);
        assert!(matches!(
            classify(&resp, Utc::now()),
            AgentLimit::Limited { .. }
        ));
    }
}
