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
    CancelToken, PiRunner, PiRunnerOptions, ResolvedAgent, RetryConfig, RunError, SeherTool,
    StreamChunk, is_client_error_retryable, is_transient_http_error, sdk_supports_tools,
    split_model_ref, split_thinking_suffix,
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
    /// number and the error message that triggered the retry.
    pub on_retry: Option<Arc<dyn Fn(u32, &str) + Send + Sync>>,
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
    },
    ClaudeHeadless {
        model: Option<String>,
    },
    ClaudeTerminal {
        model: Option<String>,
    },
    /// Unknown sdk kind — will emit a [`StreamChunk::Error`] on the channel.
    Unsupported {
        message: String,
    },
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

    Ok(match sdk {
        "pi" => {
            let (provider, model, thinking) =
                split_model_ref(&resolved.provider, &resolved.model_id);
            BackendChoice::Pi {
                provider,
                model,
                thinking,
            }
        }
        "claude" => {
            let (model_name, _) = split_thinking_suffix(&resolved.model_id);
            BackendChoice::ClaudeAgent {
                model: if model_name.is_empty() {
                    None
                } else {
                    Some(model_name.to_string())
                },
            }
        }
        "claude-headless" => {
            let (model_name, _) = split_thinking_suffix(&resolved.model_id);
            BackendChoice::ClaudeHeadless {
                model: if model_name.is_empty() {
                    None
                } else {
                    Some(model_name.to_string())
                },
            }
        }
        "claude-terminal" => {
            let (model_name, _) = split_thinking_suffix(&resolved.model_id);
            BackendChoice::ClaudeTerminal {
                model: if model_name.is_empty() {
                    None
                } else {
                    Some(model_name.to_string())
                },
            }
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
                tools: opts.tools,
            };
            PiRunner::new(pi_opts).stream(prompt, opts.resume)
        }
        Ok(BackendChoice::ClaudeAgent { model }) => {
            let config = ClaudeAgentRunnerConfig {
                model,
                system_prompt: opts.system_prompt,
                cwd: opts
                    .working_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                resume_session_id: opts.resume,
                tools: opts.tools,
                ..Default::default()
            };
            stream_agent(config, prompt, resolved.provider.clone())
        }
        Ok(BackendChoice::ClaudeHeadless { model }) => {
            let config = ClaudeHeadlessRunnerConfig {
                model,
                system_prompt: opts.system_prompt,
                cwd: opts
                    .working_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                resume_session_id: opts.resume,
                timeout_ms: opts.timeout_ms,
                cancel: opts.cancel.clone(),
                ..Default::default()
            };
            stream_headless(
                ClaudeHeadlessRunner::new(config),
                prompt,
                resolved.provider.clone(),
            )
        }
        Ok(BackendChoice::ClaudeTerminal { model }) => {
            let sdk = new_sdk_with_defaults(
                None,
                None,
                model,
                opts.system_prompt,
                opts.timeout_ms,
                opts.working_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
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

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "delay values are small configuration integers; loss/truncation is acceptable"
)]
fn calculate_retry_delay(attempt: u32, retry: &RetryConfig) -> Duration {
    let exponent = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
    let delay_secs = retry.initial_delay_secs as f64 * retry.multiplier.powi(exponent);
    let clamped = delay_secs.min(retry.max_delay_secs as f64) as u64;
    Duration::from_secs(clamped)
}

/// Internal retry loop used by [`run_for_resolved`].
///
/// The `sleep_fn` parameter lets tests swap real sleeping for a no-op.
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
                if attempt >= retry.max_attempts || !retry.enabled {
                    return Err(err);
                }
                match &err {
                    RunError::Timeout { .. } => return Err(err),
                    RunError::Limit { .. } => {}
                    RunError::Other { message, .. } => {
                        let retryable = is_transient_http_error(message)
                            || (retry.retry_client_errors && is_client_error_retryable(message));
                        if !retryable {
                            return Err(err);
                        }
                    }
                }
                if let Some(cb) = on_retry {
                    cb(attempt, &err.to_string());
                }
                let delay = calculate_retry_delay(attempt, retry);
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
///
/// # Errors
///
/// Returns [`RunError::Limit`] on rate/usage limits, [`RunError::Other`] for
/// non-retryable failures, and [`RunError::Timeout`] without retry.
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
            BackendChoice::ClaudeAgent { model } => {
                assert_eq!(model, Some("sonnet".to_string()));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
        }
    }

    #[test]
    fn choose_backend_claude_strips_thinking_suffix_from_model() {
        // Given: sdk=claude with a model that has a thinking-level suffix like :high
        // When: choose_backend is called
        // Then: ClaudeAgent gets the model WITHOUT the :high suffix
        let resolved = make_resolved("claude", "claude", "sonnet:high");
        let choice = choose_backend(&resolved, &no_tools_opts())
            .expect("claude with thinking suffix is valid");
        match choice {
            BackendChoice::ClaudeAgent { model } => {
                assert_eq!(model, Some("sonnet".to_string()));
            }
            other => panic!("expected ClaudeAgent, got {other:?}"),
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
        // Given: a channel with Session → Delta → Delta → Done(empty)
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
        // Given: a channel with Delta("partial") → Limit
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
