//! SDK-agnostic dispatch layer.
//!
//! [`stream_for_resolved`] inspects a [`ResolvedAgent`] and routes to the
//! appropriate runner backend (`pi`, `claude`, `claude-headless`,
//! `claude-terminal`). [`run_for_resolved`] wraps it with fold logic that
//! accumulates [`StreamChunk`]s into a final [`RunOutput`].
//!
//! This module centralises the dispatch logic that previously lived in
//! `seher_cli::run_mode::dispatch_stream`.

use std::sync::mpsc::Receiver;

use crate::claude_agent::{ClaudeAgentRunnerConfig, stream_agent};
use crate::claude_headless::{ClaudeHeadlessRunner, ClaudeHeadlessRunnerConfig, stream_headless};
use crate::claude_terminal::{new_sdk_with_defaults, stream_via_thread};
use crate::sdk::{
    PiRunner, PiRunnerOptions, ResolvedAgent, RunError, SeherTool, StreamChunk, sdk_supports_tools,
    split_model_ref, split_thinking_suffix,
};

/// Options forwarded to the chosen runner backend.
#[derive(Default)]
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

/// Run a prompt through the resolved SDK and return the full output.
///
/// Internally calls [`stream_for_resolved`] and folds the chunks via
/// [`fold_stream`].
///
/// # Errors
///
/// Returns [`RunError::Limit`] on rate/usage limits, [`RunError::Other`] for
/// all other failures.
pub fn run_for_resolved(
    resolved: &ResolvedAgent,
    prompt: String,
    opts: RunAgentOptions,
) -> Result<RunOutput, RunError> {
    let rx = stream_for_resolved(resolved, prompt, opts);
    fold_stream(&rx)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::channel;

    use super::*;
    use crate::sdk::config::ResolvedSkillsConfig;
    use crate::sdk::errors::LimitError;

    fn make_resolved(sdk: &str, provider: &str, model_id: &str) -> ResolvedAgent {
        ResolvedAgent {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            mode_key: "build".to_string(),
            sdk: sdk.to_string(),
            api: None,
            skills: ResolvedSkillsConfig::default(),
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
}
