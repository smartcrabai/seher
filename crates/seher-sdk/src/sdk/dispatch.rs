//! SDK-agnostic dispatch layer.
//!
//! [`stream_for_resolved`] inspects a [`ResolvedAgent`] and routes to the
//! appropriate runner backend (`pi`, `claude`, `claude-headless`,
//! `claude-terminal`). [`run_for_resolved`] wraps it with fold logic that
//! accumulates [`StreamChunk`]s into a final [`RunOutput`].
//!
//! This module centralises the dispatch logic that previously lived in
//! `seher_cli::run_mode::dispatch_stream`.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::claude_agent::{ClaudeAgentRunnerConfig, stream_agent};
use crate::claude_headless::{ClaudeHeadlessRunner, ClaudeHeadlessRunnerConfig, stream_headless};
use crate::claude_terminal::{new_sdk_with_defaults, stream_via_thread};
use crate::sdk::{
    CancelToken, EffortLevel, PiRunner, PiRunnerOptions, ResolvedAgent, RetryConfig, RunError,
    SeherTool, StreamChunk, sdk_supports_tools, split_model_ref, split_thinking_suffix,
};

/// Options forwarded to the chosen runner backend.
#[derive(Default, Clone)]
#[expect(
    clippy::type_complexity,
    reason = "callback type is intentionally simple"
)]
pub struct RunAgentOptions {
    /// Working directory the agent operates in.
    pub working_dir: Option<std::path::PathBuf>,
    /// Session id to resume (multi-turn). `None` starts a fresh session.
    pub resume: Option<String>,
    /// Custom tools (function calling). Non-empty tools require a
    /// tool-capable SDK (`pi` or `claude`); passing tools to
    /// `claude-headless` / `claude-terminal` returns a channel error.
    pub tools: Vec<SeherTool>,
    /// Override the API key resolved from `resolved.api.key`. When `None`,
    /// `resolved.api.key` is used. Only forwarded to the `pi` backend; `claude`,
    /// `claude-headless`, and `claude-terminal` use the system credential chain.
    pub api_key: Option<String>,
    /// Hard deadline for the runner process. Forwarded to `claude-headless`
    /// (`ClaudeHeadlessRunnerConfig::timeout_ms`) and `claude-terminal`
    /// (`new_sdk_with_defaults`). Has no effect on the `pi` or `claude` backends.
    pub timeout_ms: Option<u64>,
    /// Extra system-prompt text to append.
    pub system_prompt: Option<String>,
    /// Cancellation token. When [`CancelToken::cancel`] is called, the
    /// runner should abort as soon as possible. Currently forwarded to the
    /// `claude-headless` backend; other backends ignore it.
    pub cancel: CancelToken,
    /// Optional callback invoked on each retry. Receives the 1-based attempt
    /// number and a short human-readable summary of the error that triggered
    /// the retry (without the partial-output length suffix).
    pub on_retry: Option<Arc<dyn Fn(u32, &str) + Send + Sync>>,
    /// Reasoning effort level forwarded to the backend. When set, takes
    /// precedence over any effort resolved from `config.yaml` (`resolved.effort`)
    /// or a `:level` suffix on the model id (all four backends honor the
    /// suffix as a final fallback).
    pub effort: Option<EffortLevel>,
}

/// Output of a completed [`run_for_resolved`] call.
#[derive(Debug)]
pub struct RunOutput {
    pub text: String,
    pub session_id: Option<String>,
}

/// Internal representation of which backend to use and with what parameters.
/// Extracted from [`stream_for_resolved`] so routing can be unit-tested without
/// spawning real processes.
#[derive(Debug)]
pub(crate) enum BackendChoice {
    Pi {
        provider: String,
        model: String,
        thinking: Option<String>,
    },
    ClaudeAgent {
        model: Option<String>,
        effort: Option<EffortLevel>,
    },
    ClaudeHeadless {
        model: Option<String>,
        effort: Option<EffortLevel>,
    },
    ClaudeTerminal {
        model: Option<String>,
        effort: Option<EffortLevel>,
    },
    /// Unknown sdk kind -- will emit a [`StreamChunk::Error`] on the channel.
    Unsupported { message: String },
}

/// Map an [`EffortLevel`] to the `pi` backend's thinking-level string.
///
/// `pi::model::ThinkingLevel` has no `max` variant, so `EffortLevel::Max` maps
/// to pi's highest tier, `"xhigh"`. Every other variant has an identically
/// named pi thinking level.
fn effort_to_thinking(effort: EffortLevel) -> &'static str {
    match effort {
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        EffortLevel::High => "high",
        EffortLevel::XHigh | EffortLevel::Max => "xhigh",
    }
}

/// Map a recognized model-suffix thinking level to the closest [`EffortLevel`],
/// for backends (`claude`, `claude-headless`, `claude-terminal`) whose
/// `--effort` flag has no "off" tier.
///
/// Mirrors `pi::model::ThinkingLevel`'s alias vocabulary (case-insensitive,
/// including numeric aliases) so the same `:level` suffix means the same
/// thing regardless of backend. `off`/`none`/`0` have no `EffortLevel`
/// equivalent and intentionally resolve to `None` so no `--effort` flag is
/// sent, rather than guessing a tier.
fn effort_from_suffix(suffix: &str) -> Option<EffortLevel> {
    match suffix.trim().to_lowercase().as_str() {
        "minimal" | "min" | "low" | "1" => Some(EffortLevel::Low),
        "medium" | "med" | "2" => Some(EffortLevel::Medium),
        "high" | "3" => Some(EffortLevel::High),
        "xhigh" | "4" => Some(EffortLevel::XHigh),
        "max" => Some(EffortLevel::Max),
        // "off" / "none" / "0" have no EffortLevel equivalent, same as any
        // other unrecognized string.
        _ => None,
    }
}

/// Resolve the model name and effective effort for a `claude`-family backend
/// (`claude`, `claude-headless`, `claude-terminal`), which all share the same
/// logic: strip a recognized `:level` suffix from the model id, and use it as
/// the effort fallback when no explicit/resolved `effort` was set.
fn claude_family_model_and_effort(
    resolved: &ResolvedAgent,
    effort: Option<EffortLevel>,
) -> (Option<String>, Option<EffortLevel>) {
    let (model_name, suffix_thinking) = split_thinking_suffix(&resolved.model_id);
    let effort = effort.or_else(|| suffix_thinking.and_then(effort_from_suffix));
    let model = if model_name.is_empty() {
        None
    } else {
        Some(model_name.to_string())
    };
    (model, effort)
}

/// Error returned by [`choose_backend`] before any channel is opened.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DispatchError {
    /// The caller passed non-empty tools to a backend that cannot honor them.
    ToolsNotSupported { sdk: String },
}

/// Inspect `resolved.sdk` and `opts` to decide which backend to use.
///
/// Returns [`DispatchError::ToolsNotSupported`] when non-empty tools are sent
/// to a backend that cannot execute them (`claude-headless`, `claude-terminal`).
pub(crate) fn choose_backend(
    resolved: &ResolvedAgent,
    opts: &RunAgentOptions,
) -> Result<BackendChoice, DispatchError> {
    let sdk = resolved.sdk.as_str();

    if !opts.tools.is_empty() && !sdk_supports_tools(sdk) {
        return Err(DispatchError::ToolsNotSupported {
            sdk: sdk.to_string(),
        });
    }

    // The explicit `effort` field (programmatic or config-resolved) takes
    // precedence over a `:level` suffix on the model id.
    let effort = opts.effort.or(resolved.effort);

    Ok(match sdk {
        "pi" => {
            let (provider, model, suffix_thinking) =
                split_model_ref(&resolved.provider, &resolved.model_id);
            let thinking = effort
                .map(|e| effort_to_thinking(e).to_string())
                .or(suffix_thinking);
            BackendChoice::Pi {
                provider,
                model,
                thinking,
            }
        }
        "claude" => {
            let (model, effort) = claude_family_model_and_effort(resolved, effort);
            BackendChoice::ClaudeAgent { model, effort }
        }
        "claude-headless" => {
            let (model, effort) = claude_family_model_and_effort(resolved, effort);
            BackendChoice::ClaudeHeadless { model, effort }
        }
        "claude-terminal" => {
            let (model, effort) = claude_family_model_and_effort(resolved, effort);
            BackendChoice::ClaudeTerminal { model, effort }
        }
        other => BackendChoice::Unsupported {
            message: format!("unsupported sdk kind: {other}"),
        },
    })
}

/// Consume a [`Receiver<StreamChunk>`] and fold it into a [`RunOutput`].
///
/// This is the same loop that [`crate::sdk::PiRunner::run`] uses, extracted so
/// it can be tested in isolation and shared by both the pi and claude paths.
pub(crate) fn fold_stream(rx: &Receiver<StreamChunk>) -> Result<RunOutput, RunError> {
    let mut buffered = String::new();
    let mut session_id: Option<String> = None;
    loop {
        match rx.recv() {
            Ok(StreamChunk::Delta(d)) => buffered.push_str(&d),
            Ok(StreamChunk::Session(id)) => session_id = Some(id),
            Ok(StreamChunk::Done(text)) => {
                return Ok(RunOutput {
                    text: if text.is_empty() { buffered } else { text },
                    session_id,
                });
            }
            Ok(StreamChunk::Limit(error)) => {
                return Err(RunError::Limit {
                    error,
                    partial: buffered,
                });
            }
            Ok(StreamChunk::Error(msg)) => {
                return Err(RunError::Other {
                    message: msg,
                    partial: buffered,
                });
            }
            Err(_) => {
                return Err(RunError::Other {
                    message: "seher dispatch channel closed".to_string(),
                    partial: buffered,
                });
            }
        }
    }
}

/// Route `resolved` to the appropriate runner and return a streaming channel.
///
/// The caller iterates the returned [`Receiver`] to consume [`StreamChunk`]s.
/// If `opts.tools` is non-empty and `resolved.sdk` does not support tools, the
/// channel will contain a single [`StreamChunk::Error`] and then close.
#[must_use]
pub fn stream_for_resolved(
    resolved: &ResolvedAgent,
    prompt: String,
    opts: RunAgentOptions,
) -> Receiver<StreamChunk> {
    let api_key = opts
        .api_key
        .clone()
        .or_else(|| resolved.api.as_ref().and_then(|a| a.key.clone()));

    match choose_backend(resolved, &opts) {
        Err(DispatchError::ToolsNotSupported { sdk }) => {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(StreamChunk::Error(format!(
                "sdk '{sdk}' does not support custom tools"
            )));
            rx
        }
        Ok(BackendChoice::Pi {
            provider,
            model,
            thinking,
        }) => {
            let pi_opts = PiRunnerOptions {
                provider: Some(provider),
                model: Some(model),
                thinking,
                api_key,
                system_prompt: opts.system_prompt,
                working_directory: opts.working_dir,
                env: resolved.env.clone(),
                tools: opts.tools,
            };
            PiRunner::new(pi_opts).stream(prompt, opts.resume)
        }
        Ok(BackendChoice::ClaudeAgent { model, effort }) => {
            let config = ClaudeAgentRunnerConfig {
                model,
                effort,
                system_prompt: opts.system_prompt,
                cwd: opts
                    .working_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                resume_session_id: opts.resume,
                tools: opts.tools,
                env: resolved
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                ..Default::default()
            };
            stream_agent(config, prompt, resolved.provider.clone())
        }
        Ok(BackendChoice::ClaudeHeadless { model, effort }) => {
            let config = ClaudeHeadlessRunnerConfig {
                model,
                effort,
                system_prompt: opts.system_prompt,
                cwd: opts
                    .working_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                resume_session_id: opts.resume,
                timeout_ms: opts.timeout_ms,
                cancel: opts.cancel.clone(),
                env: resolved.env.clone(),
                ..Default::default()
            };
            stream_headless(
                ClaudeHeadlessRunner::new(config),
                prompt,
                resolved.provider.clone(),
            )
        }
        Ok(BackendChoice::ClaudeTerminal { model, effort }) => {
            let sdk = new_sdk_with_defaults(
                None,
                None,
                model,
                opts.system_prompt,
                effort,
                opts.timeout_ms,
                opts.working_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                resolved
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<std::collections::HashMap<_, _>>(),
            );
            stream_via_thread(sdk, prompt, resolved.provider.clone(), opts.resume)
        }
        Ok(BackendChoice::Unsupported { message }) => {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(StreamChunk::Error(message));
            rx
        }
    }
}

/// Internal retry loop used by [`run_for_resolved`].
///
/// Retries [`RunError::Limit`] (rate/usage limits are always transient) and
/// [`RunError::Other`] messages classified as transient HTTP errors. Note that
/// [`RunError::Limit`] is retried against the *same* provider; callers that want
/// provider fallback should handle the limit error themselves or use the async
/// resolution path.
/// [`RunError::Timeout`] is surfaced immediately so callers can handle timeout
/// configuration themselves.
///
/// The `sleep_fn` parameter lets tests swap real sleeping for a no-op. The real
/// implementation uses [`std::thread::sleep`], which blocks the calling thread.
#[expect(
    clippy::needless_pass_by_value,
    clippy::type_complexity,
    reason = "mirrors the public run_for_resolved signature; callback type is intentionally simple"
)]
pub(crate) fn run_with_retry_inner<F, S>(
    mut run: F,
    prompt: String,
    opts: RunAgentOptions,
    retry: &RetryConfig,
    on_retry: Option<&dyn Fn(u32, &str)>,
    mut sleep_fn: S,
) -> Result<RunOutput, RunError>
where
    F: FnMut(String, RunAgentOptions) -> Result<RunOutput, RunError>,
    S: FnMut(Duration),
{
    let mut attempt = 1;
    loop {
        let result = run(prompt.clone(), opts.clone());
        match result {
            Ok(output) => return Ok(output),
            Err(err) => {
                if attempt >= retry.effective_max_attempts() || !retry.enabled {
                    return Err(err);
                }
                match &err {
                    RunError::Timeout { .. } => return Err(err),
                    RunError::Limit { .. } => {}
                    RunError::Other { message, .. } => {
                        if !retry.is_retryable_message(message) {
                            return Err(err);
                        }
                    }
                }
                let retry_message = match &err {
                    RunError::Other { message, .. } => message.clone(),
                    RunError::Limit { error, .. } => error.to_string(),
                    RunError::Timeout { error, .. } => error.to_string(),
                };
                if let Some(cb) = on_retry {
                    cb(attempt, &retry_message);
                }
                let delay = retry.delay_for_attempt(attempt);
                sleep_fn(delay);
                attempt += 1;
            }
        }
    }
}

/// Run a prompt through the resolved SDK and return the full output.
///
/// Internally calls [`stream_for_resolved`] and folds the chunks via
/// [`fold_stream`], retrying transient failures according to `resolved.retry`.
/// This function is synchronous and blocks the calling thread while waiting
/// between retry attempts. Rate-limit errors are retried against the *same*
/// provider; use the async resolution APIs if you need provider fallback.
///
/// # Errors
///
/// Returns [`RunError::Limit`] after retrying with exponential backoff,
/// [`RunError::Other`] for non-retryable failures, and [`RunError::Timeout`]
/// without retry.
pub fn run_for_resolved(
    resolved: &ResolvedAgent,
    prompt: String,
    opts: RunAgentOptions,
) -> Result<RunOutput, RunError> {
    let on_retry_holder = opts.on_retry.clone();
    #[expect(
        clippy::type_complexity,
        reason = "callback type is intentionally simple"
    )]
    let on_retry: Option<&dyn Fn(u32, &str)> =
        on_retry_holder.as_deref().map(|f| f as &dyn Fn(u32, &str));
    run_with_retry_inner(
        |p, o| {
            let rx = stream_for_resolved(resolved, p, o);
            fold_stream(&rx)
        },
        prompt,
        opts,
        &resolved.retry,
        on_retry,
        std::thread::sleep,
    )
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::channel;

    use super::*;
    use crate::sdk::config::{ResolvedSkillsConfig, RetryConfig};
    use crate::sdk::errors::{LimitError, TimeoutError};

    fn make_resolved(sdk: &str, provider: &str, model_id: &str) -> ResolvedAgent {
        ResolvedAgent {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            mode_key: "build".to_string(),
            sdk: sdk.to_string(),
            api: None,
            skills: ResolvedSkillsConfig::default(),
            retry: RetryConfig::default(),
            env: indexmap::IndexMap::new(),
            effort: None,
        }
    }

    fn dummy_tool() -> SeherTool {
        SeherTool::new(
            "dummy",
            "dummy tool",
            serde_json::json!({"type": "object", "properties": {}}),
            Arc::new(|_| Ok(String::new())),
        )
    }

    fn no_tools_opts() -> RunAgentOptions {
        RunAgentOptions::default()
    }

    fn tools_opts() -> RunAgentOptions {
        RunAgentOptions {
            tools: vec![dummy_tool()],
            ..Default::default()
        }
    }

    // -- choose_backend: pi routing ------------------------------------------

    #[test]
    fn choose_backend_pi_extracts_provider_model_and_thinking() {
        // Given: sdk=pi with a compound model_id that includes provider and thinking level
        // When: choose_backend is called
        // Then: Pi backend is selected with correct provider, model, and thinking fields
        let resolved = make_resolved("pi", "codex", "openai-codex/gpt-5.5:xhigh");
        let choice =
            choose_backend(&resolved, &no_tools_opts()).expect("pi backend is always valid");
        match choice {
            BackendChoice::Pi {
                provider,
                model,
                thinking,
            } => {
                assert_eq!(provider, "openai-codex");
                assert_eq!(model, "gpt-5.5");
                assert_eq!(thinking, Some("xhigh".to_string()));
            }
            other => panic!("expected Pi, got {other:?}"),
        }
    }

    // -- choose_backend: claude routing --------------------------------------

    #[test]
    fn choose_backend_claude_sets_bare_model_name() {
        // Given: sdk=claude with a bare model name (no thinking suffix)
        // When: choose_backend is called
        // Then: ClaudeAgent backend is selected with the model as-is
        // This is the regression guard: sdk=claude must NOT be routed to PiRunner
        let resolved = make_resolved("claude", "claude", "sonnet");
        let choice =
            choose_backend(&resolved, &no_tools_opts()).expect("claude backend is always valid");
        match choice {
            BackendChoice::ClaudeAgent { model, effort: _ } => {
                assert_eq!(model, Some("sonnet".to_string()));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_strips_thinking_suffix_from_model() {
        // Given: sdk=claude with a model that has a thinking-level suffix like :high
        // When: choose_backend is called
        // Then: ClaudeAgent gets the model WITHOUT the :high suffix, and the
        // suffix is used as the effort fallback since no explicit/resolved
        // effort was set
        let resolved = make_resolved("claude", "claude", "sonnet:high");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude with thinking suffix is valid");
        match choice {
            BackendChoice::ClaudeAgent { model, effort } => {
                assert_eq!(model, Some("sonnet".to_string()));
                assert_eq!(effort, Some(EffortLevel::High));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_explicit_effort_overrides_suffix_thinking() {
        // Given: sdk=claude with a model suffix ":low" but opts.effort=Max
        // When: choose_backend is called
        // Then: the explicit effort wins over the suffix-derived value
        let resolved = make_resolved("claude", "claude", "sonnet:low");
        let opts = RunAgentOptions {
            effort: Some(EffortLevel::Max),
            ..Default::default()
        };
        let choice = choose_backend(&resolved, &opts).expect("claude with effort is valid");
        match choice {
            BackendChoice::ClaudeAgent { effort, .. } => {
                assert_eq!(effort, Some(EffortLevel::Max));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_off_suffix_has_no_effort_equivalent() {
        // Given: sdk=claude with a suffix recognized by pi's ThinkingLevel
        // (":off" means "no extra thinking") but the claude CLI's --effort
        // flag has no "off" tier
        // When: choose_backend is called
        // Then: the model suffix is still stripped, but effort stays None
        // rather than erroring or guessing a tier
        let resolved = make_resolved("claude", "claude", "sonnet:off");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude with off suffix is still valid");
        match choice {
            BackendChoice::ClaudeAgent { model, effort } => {
                assert_eq!(model, Some("sonnet".to_string()));
                assert_eq!(effort, None);
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_med_alias_maps_to_effort_medium() {
        // Given: sdk=claude with pi's "med" alias suffix (not the literal
        // EffortLevel string "medium")
        // When: choose_backend is called
        // Then: it still resolves to EffortLevel::Medium via the alias-aware
        // suffix mapping, not silently dropped
        let resolved = make_resolved("claude", "claude", "sonnet:med");
        let choice =
            choose_backend(&resolved, &no_tools_opts()).expect("claude with med alias is valid");
        match choice {
            BackendChoice::ClaudeAgent { model, effort } => {
                assert_eq!(model, Some("sonnet".to_string()));
                assert_eq!(effort, Some(EffortLevel::Medium));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_max_suffix_maps_to_effort_max() {
        // Given: sdk=claude with a ":max" suffix -- not a pi::ThinkingLevel
        // at all, but a valid EffortLevel
        // When: choose_backend is called
        // Then: the suffix is recognized, stripped from the model name, and
        // mapped to EffortLevel::Max (regression test for the model-id
        // corruption bug where ":max" was previously left attached)
        let resolved = make_resolved("claude", "claude", "sonnet:max");
        let choice =
            choose_backend(&resolved, &no_tools_opts()).expect("claude with max suffix is valid");
        match choice {
            BackendChoice::ClaudeAgent { model, effort } => {
                assert_eq!(model, Some("sonnet".to_string()));
                assert_eq!(effort, Some(EffortLevel::Max));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_headless_uses_suffix_thinking_as_effort() {
        // Given: sdk=claude-headless with a model suffix ":xhigh"
        // When: choose_backend is called
        // Then: ClaudeHeadless gets the suffix-derived effort
        let resolved = make_resolved("claude-headless", "claude", "sonnet:xhigh");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude-headless with thinking suffix is valid");
        match choice {
            BackendChoice::ClaudeHeadless { effort, .. } => {
                assert_eq!(effort, Some(EffortLevel::XHigh));
            }
            other => panic!("expected ClaudeHeadless, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_terminal_uses_suffix_thinking_as_effort() {
        // Given: sdk=claude-terminal with a model suffix ":medium"
        // When: choose_backend is called
        // Then: ClaudeTerminal gets the suffix-derived effort
        let resolved = make_resolved("claude-terminal", "claude", "sonnet:medium");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude-terminal with thinking suffix is valid");
        match choice {
            BackendChoice::ClaudeTerminal { effort, .. } => {
                assert_eq!(effort, Some(EffortLevel::Medium));
            }
            other => panic!("expected ClaudeTerminal, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_agent_carries_resolved_effort() {
        // Given: sdk=claude with no opts.effort set, but resolved.effort from config
        // When: choose_backend is called
        // Then: ClaudeAgent gets the config-resolved effort level
        let mut resolved = make_resolved("claude", "claude", "sonnet");
        resolved.effort = Some(EffortLevel::High);
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude with resolved effort is valid");
        match choice {
            BackendChoice::ClaudeAgent { effort, .. } => {
                assert_eq!(effort, Some(EffortLevel::High));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_explicit_effort_overrides_resolved_effort() {
        // Given: sdk=claude with both opts.effort and resolved.effort set
        // When: choose_backend is called
        // Then: the explicit opts.effort wins
        let mut resolved = make_resolved("claude", "claude", "sonnet");
        resolved.effort = Some(EffortLevel::Low);
        let opts = RunAgentOptions {
            effort: Some(EffortLevel::Max),
            ..Default::default()
        };
        let choice = choose_backend(&resolved, &opts).expect("claude with effort is valid");
        match choice {
            BackendChoice::ClaudeAgent { effort, .. } => {
                assert_eq!(effort, Some(EffortLevel::Max));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_pi_explicit_effort_overrides_suffix_thinking() {
        // Given: sdk=pi with a model suffix ":low" but opts.effort=Max
        // When: choose_backend is called
        // Then: the explicit effort wins and maps to pi's "xhigh" thinking level
        let resolved = make_resolved("pi", "codex", "openai-codex/gpt-5.5:low");
        let opts = RunAgentOptions {
            effort: Some(EffortLevel::Max),
            ..Default::default()
        };
        let choice = choose_backend(&resolved, &opts).expect("pi backend is always valid");
        match choice {
            BackendChoice::Pi { thinking, .. } => {
                assert_eq!(thinking, Some("xhigh".to_string()));
            }
            other => panic!("expected Pi, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_pi_recognizes_max_suffix_but_pi_has_no_max_tier() {
        // Given: sdk=pi with a ":max" model suffix -- recognized as a suffix
        // (so the model name is correctly stripped) even though pi's own
        // ThinkingLevel has no "max" tier
        // When: choose_backend is called
        // Then: the model id is NOT corrupted with a literal ":max" left
        // attached; the raw "max" string is passed through as `thinking` and
        // will surface a clear runtime error from pi_runner's own validation
        // (parse_thinking) rather than reaching pi with a broken model id
        let resolved = make_resolved("pi", "codex", "openai-codex/gpt-5.5:max");
        let choice =
            choose_backend(&resolved, &no_tools_opts()).expect("pi backend is always valid");
        match choice {
            BackendChoice::Pi {
                provider,
                model,
                thinking,
            } => {
                assert_eq!(provider, "openai-codex");
                assert_eq!(model, "gpt-5.5");
                assert_eq!(thinking, Some("max".to_string()));
            }
            other => panic!("expected Pi, got {other:?}"),
        }
    }

    // -- choose_backend: claude-headless routing -----------------------------

    #[test]
    fn choose_backend_claude_headless_routes_when_no_tools() {
        // Given: sdk=claude-headless with no tools
        // When: choose_backend is called
        // Then: ClaudeHeadless backend is selected
        let resolved = make_resolved("claude-headless", "claude", "sonnet");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude-headless with no tools is valid");
        assert!(matches!(choice, BackendChoice::ClaudeHeadless { .. }));
    }

    #[test]
    fn choose_backend_claude_headless_errors_when_tools_non_empty() {
        // Given: sdk=claude-headless with non-empty tools
        // When: choose_backend is called
        // Then: DispatchError::ToolsNotSupported is returned (headless cannot honor tools)
        let resolved = make_resolved("claude-headless", "claude", "sonnet");
        let err = choose_backend(&resolved, &tools_opts())
            .expect_err("claude-headless does not support tools");
        assert_eq!(
            err,
            DispatchError::ToolsNotSupported {
                sdk: "claude-headless".to_string()
            }
        );
    }

    // -- choose_backend: claude-terminal routing -----------------------------

    #[test]
    fn choose_backend_claude_terminal_routes_when_no_tools() {
        // Given: sdk=claude-terminal with no tools
        // When: choose_backend is called
        // Then: ClaudeTerminal backend is selected
        let resolved = make_resolved("claude-terminal", "claude", "sonnet");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude-terminal with no tools is valid");
        assert!(matches!(choice, BackendChoice::ClaudeTerminal { .. }));
    }

    #[test]
    fn choose_backend_claude_terminal_errors_when_tools_non_empty() {
        // Given: sdk=claude-terminal with non-empty tools
        // When: choose_backend is called
        // Then: DispatchError::ToolsNotSupported is returned
        let resolved = make_resolved("claude-terminal", "claude", "sonnet");
        let err = choose_backend(&resolved, &tools_opts())
            .expect_err("claude-terminal does not support tools");
        assert_eq!(
            err,
            DispatchError::ToolsNotSupported {
                sdk: "claude-terminal".to_string()
            }
        );
    }

    // -- choose_backend: unknown sdk kind ------------------------------------

    #[test]
    fn choose_backend_unknown_sdk_returns_unsupported_variant() {
        // Given: sdk with an unrecognized kind
        // When: choose_backend is called
        // Then: BackendChoice::Unsupported is returned (NOT a panic or Err)
        let resolved = make_resolved("unknown-foo", "unknown-foo", "some-model");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("unknown sdk returns Unsupported variant, not Err");
        assert!(matches!(choice, BackendChoice::Unsupported { .. }));
    }

    // -- stream_for_resolved: unknown sdk emits channel error ----------------

    #[test]
    fn stream_for_resolved_unknown_sdk_sends_error_chunk_then_closes() {
        // Given: a resolved agent with an unsupported sdk kind
        // When: stream_for_resolved is called
        // Then: the channel yields exactly one StreamChunk::Error and then closes (no panic)
        let resolved = make_resolved("unknown-foo", "unknown-foo", "some-model");
        let rx = stream_for_resolved(&resolved, "prompt".to_string(), no_tools_opts());
        let chunk = rx.recv().expect("at least one chunk on unknown sdk");
        assert!(
            matches!(chunk, StreamChunk::Error(_)),
            "expected Error chunk, got {chunk:?}"
        );
        // Channel should be closed after the single Error chunk
        assert!(
            rx.recv().is_err(),
            "channel must close after the error chunk"
        );
    }

    #[test]
    fn stream_for_resolved_tools_on_headless_sdk_sends_error_chunk() {
        // Given: sdk=claude-headless with tools
        // When: stream_for_resolved is called
        // Then: the channel yields a StreamChunk::Error and closes (not a panic)
        let resolved = make_resolved("claude-headless", "claude", "sonnet");
        let rx = stream_for_resolved(&resolved, "prompt".to_string(), tools_opts());
        let chunk = rx.recv().expect("error chunk for tools-on-headless");
        assert!(
            matches!(chunk, StreamChunk::Error(_)),
            "expected Error chunk, got {chunk:?}"
        );
        assert!(
            rx.recv().is_err(),
            "channel must close after the error chunk"
        );
    }

    // -- fold_stream: accumulation and termination ---------------------------

    #[test]
    fn fold_stream_accumulates_deltas_and_returns_on_done() {
        // Given: a channel with Session -> Delta -> Delta -> Done(empty)
        // When: fold_stream consumes the channel
        // Then: RunOutput has the concatenated text and the session id
        let (tx, rx) = channel();
        tx.send(StreamChunk::Session("s1".to_string()))
            .expect("send Session");
        tx.send(StreamChunk::Delta("hi ".to_string()))
            .expect("send Delta 1");
        tx.send(StreamChunk::Delta("there".to_string()))
            .expect("send Delta 2");
        tx.send(StreamChunk::Done(String::new()))
            .expect("send Done");
        drop(tx);

        let out = fold_stream(&rx).expect("fold succeeds");
        assert_eq!(out.text, "hi there");
        assert_eq!(out.session_id, Some("s1".to_string()));
    }

    #[test]
    fn fold_stream_done_with_non_empty_text_takes_precedence_over_buffered() {
        // Given: a channel with Delta then Done("full text") where Done text is non-empty
        // When: fold_stream consumes the channel
        // Then: RunOutput.text == "full text" (Done text wins over accumulated deltas)
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("ignored delta".to_string()))
            .expect("send Delta");
        tx.send(StreamChunk::Done("full text".to_string()))
            .expect("send Done");
        drop(tx);

        let out = fold_stream(&rx).expect("fold succeeds");
        assert_eq!(out.text, "full text");
    }

    #[test]
    fn fold_stream_limit_returns_err_with_partial_text() {
        // Given: a channel with Delta("partial") -> Limit
        // When: fold_stream consumes the channel
        // Then: Err(RunError::Limit { partial: "partial", .. })
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("partial".to_string()))
            .expect("send Delta");
        tx.send(StreamChunk::Limit(LimitError {
            provider: "claude".to_string(),
            reset_at: None,
        }))
        .expect("send Limit");
        drop(tx);

        let err = fold_stream(&rx).expect_err("fold returns Err on Limit");
        assert!(
            matches!(err, RunError::Limit { ref partial, .. } if partial == "partial"),
            "expected Limit with partial='partial', got {err:?}"
        );
    }

    #[test]
    fn fold_stream_channel_close_without_done_returns_err() {
        // Given: a channel where the sender is dropped after Delta without Done
        // When: fold_stream consumes the channel
        // Then: Err(RunError::Other { partial: "x", .. }) with a channel-closed message
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("x".to_string()))
            .expect("send Delta");
        drop(tx); // sender dropped: simulates unexpected channel close

        let err = fold_stream(&rx).expect_err("fold returns Err on channel close");
        match &err {
            RunError::Other { message, partial } => {
                assert_eq!(partial, "x");
                assert!(
                    message.contains("channel") || message.contains("closed"),
                    "message should mention channel closure, got: {message}"
                );
            }
            other => panic!("expected RunError::Other, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // run_with_retry_inner
    // -----------------------------------------------------------------------

    fn retry_config_with_client_errors() -> RetryConfig {
        RetryConfig {
            retry_client_errors: true,
            ..RetryConfig::default()
        }
    }

    fn other_error(message: &str) -> RunError {
        RunError::Other {
            message: message.to_string(),
            partial: String::new(),
        }
    }

    fn limit_error() -> RunError {
        RunError::Limit {
            error: LimitError {
                provider: "claude".to_string(),
                reset_at: None,
            },
            partial: String::new(),
        }
    }

    fn timeout_error() -> RunError {
        RunError::Timeout {
            error: TimeoutError {
                ms: 1000,
                label: "test",
            },
            partial: String::new(),
        }
    }

    #[test]
    fn retry_succeeds_on_first_attempt() {
        // Given: a run that always succeeds
        // When: run_with_retry_inner is called
        // Then: it returns Ok immediately with no retries
        let run = |_prompt: String, _opts: RunAgentOptions| {
            Ok(RunOutput {
                text: "hello".to_string(),
                session_id: None,
            })
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            None,
            |_| {},
        );
        assert_eq!(result.unwrap().text, "hello");
    }

    #[test]
    fn retry_limit_error_then_success() {
        // Given: a run that fails with Limit once then succeeds
        // When: run_with_retry_inner is called
        // Then: it retries once and returns Ok
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            if calls == 1 {
                Err(limit_error())
            } else {
                Ok(RunOutput {
                    text: "ok".to_string(),
                    session_id: None,
                })
            }
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            None,
            |_| {},
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(calls, 2);
    }

    #[test]
    fn retry_401_with_client_errors_enabled_then_success() {
        // Given: retry_client_errors is true and a 401 occurs once
        // When: run_with_retry_inner is called
        // Then: it retries and succeeds
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            if calls == 1 {
                Err(other_error("Anthropic API error (HTTP 401): auth_error"))
            } else {
                Ok(RunOutput {
                    text: "ok".to_string(),
                    session_id: None,
                })
            }
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &retry_config_with_client_errors(),
            None,
            |_| {},
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(calls, 2);
    }

    #[test]
    fn retry_401_without_client_errors_fails_immediately() {
        // Given: retry_client_errors is false (default) and a 401 occurs
        // When: run_with_retry_inner is called
        // Then: it returns the error without retrying
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            Err(other_error("Anthropic API error (HTTP 401): auth_error"))
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            None,
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_404_with_client_errors_enabled_then_success() {
        // Given: retry_client_errors is true and a 404 occurs once
        // When: run_with_retry_inner is called
        // Then: it retries and succeeds
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            if calls == 1 {
                Err(other_error("Anthropic API error (HTTP 404): not found"))
            } else {
                Ok(RunOutput {
                    text: "ok".to_string(),
                    session_id: None,
                })
            }
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &retry_config_with_client_errors(),
            None,
            |_| {},
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(calls, 2);
    }

    #[test]
    fn retry_404_without_client_errors_fails_immediately() {
        // Given: retry_client_errors is false (default) and a 404 occurs
        // When: run_with_retry_inner is called
        // Then: it returns the error without retrying
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            Err(other_error("Anthropic API error (HTTP 404): not found"))
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            None,
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_500_always_retries_regardless_of_client_error_flag() {
        // Given: a 500 occurs once and retry_client_errors is false
        // When: run_with_retry_inner is called
        // Then: it retries anyway because 5xx are always transient
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            if calls == 1 {
                Err(other_error("Anthropic API error (HTTP 500): internal"))
            } else {
                Ok(RunOutput {
                    text: "ok".to_string(),
                    session_id: None,
                })
            }
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            None,
            |_| {},
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(calls, 2);
    }

    #[test]
    fn retry_403_fails_immediately() {
        // Given: a 403 occurs
        // When: run_with_retry_inner is called
        // Then: it returns the error without retrying
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            Err(other_error("Anthropic API error (HTTP 403): forbidden"))
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &retry_config_with_client_errors(),
            None,
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_disabled_fails_immediately_on_transient_error() {
        // Given: retries are disabled and a transient 429 occurs
        // When: run_with_retry_inner is called
        // Then: it returns the error without retrying
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            Err(other_error("Anthropic API error (HTTP 429): rate limited"))
        };
        let cfg = RetryConfig {
            enabled: false,
            ..RetryConfig::default()
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &cfg,
            None,
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_exhausted_returns_last_error() {
        // Given: every attempt fails with a transient error and max_attempts is 3
        // When: run_with_retry_inner is called
        // Then: it returns the last error after 3 attempts
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            Err(other_error(&format!("HTTP 500 attempt {calls}")))
        };
        let cfg = RetryConfig {
            max_attempts: 3,
            ..RetryConfig::default()
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &cfg,
            None,
            |_| {},
        );
        match result {
            Err(RunError::Other { message, .. }) => {
                assert!(message.contains("attempt 3"), "got: {message}");
            }
            other => panic!("expected attempt 3 error, got {other:?}"),
        }
        assert_eq!(calls, 3);
    }

    #[test]
    fn retry_timeout_fails_immediately() {
        // Given: a timeout occurs
        // When: run_with_retry_inner is called
        // Then: it returns the error without retrying
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            Err(timeout_error())
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            None,
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_delay_doubles_each_attempt_until_max() {
        // Given: a run that always fails transiently and a retry config with
        // initialDelaySecs=2, multiplier=2, maxDelaySecs=60
        // When: run_with_retry_inner is called
        // Then: sleep durations are 2s, 4s, 8s, 16s, ... clamped at 60s
        let run = |_prompt: String, _opts: RunAgentOptions| Err(other_error("HTTP 500"));
        let mut sleeps: Vec<u64> = Vec::new();
        let cfg = RetryConfig {
            max_attempts: 6,
            ..RetryConfig::default()
        };
        let _ = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &cfg,
            None,
            |d| sleeps.push(d.as_secs()),
        );
        assert_eq!(sleeps, vec![2, 4, 8, 16, 32]);
    }

    #[test]
    fn retry_delay_clamps_at_max_delay_secs() {
        // Given: a retry config with a low maxDelaySecs
        // When: run_with_retry_inner is called repeatedly
        // Then: sleep durations never exceed maxDelaySecs
        let run = |_prompt: String, _opts: RunAgentOptions| Err(other_error("HTTP 500"));
        let mut sleeps: Vec<u64> = Vec::new();
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_delay_secs: 10,
            max_delay_secs: 15,
            multiplier: 2.0,
            ..RetryConfig::default()
        };
        let _ = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &cfg,
            None,
            |d| sleeps.push(d.as_secs()),
        );
        assert_eq!(sleeps, vec![10, 15, 15, 15]);
    }

    #[test]
    fn retry_invokes_on_callback_once_per_retry() {
        // Given: a run that fails once then succeeds and an on_retry callback
        // When: run_with_retry_inner is called
        // Then: the callback is invoked exactly once with attempt=1
        let mut calls = 0;
        let run = |_prompt: String, _opts: RunAgentOptions| {
            calls += 1;
            if calls == 1 {
                Err(other_error("HTTP 500"))
            } else {
                Ok(RunOutput {
                    text: "ok".to_string(),
                    session_id: None,
                })
            }
        };
        let callback_attempts = std::cell::RefCell::new(Vec::new());
        let cb = |attempt: u32, _msg: &str| {
            callback_attempts.borrow_mut().push(attempt);
        };
        let result = run_with_retry_inner(
            run,
            "prompt".to_string(),
            RunAgentOptions::default(),
            &RetryConfig::default(),
            Some(&cb),
            |_| {},
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(*callback_attempts.borrow(), vec![1]);
    }
}
