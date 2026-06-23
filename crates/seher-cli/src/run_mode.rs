//! Shared "resolve + stream prompt through pi" flow used by both build and plan modes.
//!
//! Implements the retry-on-limit loop: on a `LimitError`, the resolved YAML
//! provider name is added to `exclude_providers` and resolution is retried.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use seher::claude_terminal::{default_transcript_root, encode_transcript_path};
use seher::sdk::{
    CancelToken, CodexBarProbe, Config, ResolveOptions, ResolvedAgent, RunAgentOptions,
    TimeoutError, load_config, pi_session_path, resolve_agent, stream_for_resolved,
    unsupported_sdk_providers,
};

use crate::args::Args;
use crate::logger::Logger;
use crate::stream::{Outcome, drain_to_stdout};

/// Run the full "resolve → stream" loop, returning the concatenated assistant
/// text on success.
///
/// # Errors
///
/// Returns a stringified error for resolve / timeout / non-limit pi errors.
pub fn resolve_and_stream(
    rt: &tokio::runtime::Runtime,
    prompt: &str,
    args: &Args,
    mode_key: &str,
    system_prompt: Option<&str>,
    logger: &Logger,
) -> Result<String, String> {
    // Load config + detect browser session once; reuse across retry attempts.
    let config: Config = load_config(args.config.as_deref()).map_err(|e| e.to_string())?;

    // One-time warning for non-executable providers (e.g. `sdk: codex` /
    // `sdk: copilot` from seher-ts configs). They are silently filtered out
    // of candidates by the resolver; we surface the skip here so the user
    // knows why.
    for (provider, sdk) in unsupported_sdk_providers(&config) {
        logger.warn(&format!(
            "Skipping provider '{provider}' (sdk='{sdk}'): not supported by this build (supported: 'pi', 'claude', 'claude-terminal', 'claude-headless')"
        ));
    }

    // Shared cancel token — signalled by the SIGINT handler below so that
    // all streaming paths (drain_to_stdout, headless runner) can observe it.
    let cancel = CancelToken::new();
    let cancel_for_signal = cancel.clone();
    // Spawn a dedicated thread with its own tokio runtime to listen for
    // Ctrl-C. The signal handling code must run inside a tokio context but
    // the main CLI loop is synchronous, so we isolate it here.
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel_for_signal.cancel();
            }
        });
    });

    // Resuming pins to the backend that owns the session — the retry-on-limit provider
    // switch is disabled, since a session id is meaningless to a different backend.
    if let Some(resume_id) = args.resume.clone() {
        return resume_and_stream(
            rt,
            prompt,
            args,
            mode_key,
            system_prompt,
            logger,
            &config,
            &resume_id,
            &cancel,
        );
    }

    let resolver = |excluded: &[String]| -> Result<ResolvedAgent, String> {
        resolve_once(rt, args, mode_key, excluded, &config)
    };
    let stream_runner = |resolved: &ResolvedAgent| -> Outcome {
        stream_with_http_retry(resolved, prompt, system_prompt, args, None, &cancel, logger)
    };
    stream_with_retry(args.timeout, logger, resolver, stream_runner)
}

/// Effective working directory for this run: the canonicalized `--cwd` if given,
/// otherwise the canonicalized process cwd. Used to locate session storage.
fn effective_cwd(args: &Args) -> String {
    args.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .map_or_else(|_| ".".to_string(), |p| p.to_string_lossy().into_owned())
    })
}

/// Whether two SDK backends are compatible for session resume. All claude-based
/// backends (`claude`, `claude-terminal`, `claude-headless`) share the same
/// Claude CLI transcript storage and can resume each other's sessions.
fn sdk_backends_compatible(resolved_sdk: &str, pinned_sdk: &str) -> bool {
    const CLAUDE_SDKS: &[&str] = &["claude", "claude-terminal", "claude-headless"];
    resolved_sdk == pinned_sdk
        || (CLAUDE_SDKS.contains(&resolved_sdk) && CLAUDE_SDKS.contains(&pinned_sdk))
}

/// Detect which backend owns a session id by probing on-disk storage under `cwd`.
/// Returns the sdk kind (`"claude-terminal"` / `"claude-headless"` / `"pi"`) or
/// `None` if no backend has it.
///
/// Both `claude-terminal` and `claude-headless` use the same Claude CLI transcript
/// storage, so a transcript hit could belong to either. We return
/// `"claude-terminal"` as the default for that path; if the resolver selected
/// `"claude-headless"`, the resume pinning in `resume_and_stream` accepts both
/// claude-based backends interchangeably.
fn probe_session_backend(cwd: &str, session_id: &str) -> Option<&'static str> {
    let claude_path = encode_transcript_path(&default_transcript_root(), cwd, session_id);
    if std::path::Path::new(&claude_path).exists() {
        return Some("claude-terminal");
    }
    let pi_path = pi_session_path(Some(std::path::Path::new(cwd)), session_id);
    if pi_path.exists() {
        return Some("pi");
    }
    None
}

/// Resume an existing session. Probes storage to pin the owning backend, resolves a
/// matching agent, and runs a single attempt (no provider-switch retry).
///
/// # Errors
///
/// Errors if the session is not found, the resolver selects a different backend, or the
/// run hits a limit / error / timeout.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors resolve_and_stream inputs"
)]
fn resume_and_stream(
    rt: &tokio::runtime::Runtime,
    prompt: &str,
    args: &Args,
    mode_key: &str,
    system_prompt: Option<&str>,
    logger: &Logger,
    config: &Config,
    resume_id: &str,
    cancel: &CancelToken,
) -> Result<String, String> {
    let cwd = effective_cwd(args);
    let pinned = probe_session_backend(&cwd, resume_id).ok_or_else(|| {
        format!("session '{resume_id}' not found under cwd '{cwd}' (resume requires the same --cwd used to create it)")
    })?;

    let resolved = resolve_once(rt, args, mode_key, &[], config)?;
    if !sdk_backends_compatible(&resolved.sdk, pinned) {
        return Err(format!(
            "resumed session '{resume_id}' belongs to backend '{pinned}', but the resolver selected '{}' (provider '{}') — it may be rate-limited or lower priority; pass --provider to force the matching one",
            resolved.sdk, resolved.provider
        ));
    }
    logger.info(&format!(
        "Resuming session {resume_id} on provider: {} ({}/{})",
        resolved.provider, resolved.sdk, resolved.model_id
    ));

    match stream_with_http_retry(
        &resolved,
        prompt,
        system_prompt,
        args,
        Some(resume_id),
        cancel,
        logger,
    ) {
        Outcome::Done(t) => Ok(t),
        Outcome::Limit => Err(format!(
            "provider '{}' is rate-limited; cannot switch providers while resuming session {resume_id}",
            resolved.provider
        )),
        Outcome::Error(message) => Err(message),
        Outcome::Timeout => Err(TimeoutError {
            ms: args.timeout.unwrap_or(0),
            label: "stream",
        }
        .to_string()),
        Outcome::Cancelled => Err("cancelled".to_string()),
    }
}

/// Retry-on-limit loop. Pure: takes `resolver` (produces a `ResolvedAgent` given
/// the current excluded set) and `stream_runner` (produces an `Outcome` for a
/// resolved agent). Used by both production and tests.
///
/// On `Outcome::Limit`, the resolved YAML provider name is added to `excluded`
/// and `resolver` is called again. The loop exits with `Done` text on success
/// or a stringified error on terminal failure.
///
/// # Errors
///
/// Returns a stringified error when `resolver` fails, the stream errors out,
/// or the stream times out.
pub fn stream_with_retry<R, S>(
    timeout_ms: Option<u64>,
    logger: &Logger,
    mut resolver: R,
    mut stream_runner: S,
) -> Result<String, String>
where
    R: FnMut(&[String]) -> Result<ResolvedAgent, String>,
    S: FnMut(&ResolvedAgent) -> Outcome,
{
    let mut excluded: Vec<String> = Vec::new();
    loop {
        let agent = resolver(&excluded)?;
        logger.info(&format!(
            "Selected provider: {} ({}/{})",
            agent.provider, agent.sdk, agent.model_id
        ));
        match stream_runner(&agent) {
            Outcome::Done(t) => return Ok(t),
            Outcome::Limit => {
                logger.warn(&format!(
                    "Provider '{}' hit API limit; retrying with next provider...",
                    agent.provider
                ));
                // Exclude by the resolved YAML provider name (matches what
                // `resolve_agent` compares against), not by pi's provider id
                // which lives in a different namespace.
                if !excluded.contains(&agent.provider) {
                    excluded.push(agent.provider.clone());
                }
            }
            Outcome::Error(message) => return Err(message),
            Outcome::Timeout => {
                return Err(TimeoutError {
                    ms: timeout_ms.unwrap_or(0),
                    label: "stream",
                }
                .to_string());
            }
            Outcome::Cancelled => return Err("cancelled".to_string()),
        }
    }
}

/// Short polling interval used when sleeping so that cancellation is observed
/// promptly instead of waiting for the full backoff delay.
const RETRY_SLEEP_POLL: Duration = Duration::from_millis(50);

/// Sleep for `duration`, returning early if `cancel` is signalled.
fn sleep_with_cancel(duration: Duration, cancel: &CancelToken) {
    let start = Instant::now();
    while let Some(remaining) = duration.checked_sub(start.elapsed()) {
        if cancel.is_cancelled() {
            return;
        }
        std::thread::sleep(RETRY_SLEEP_POLL.min(remaining));
    }
}

/// Run the streaming path for a resolved provider, retrying transient HTTP
/// errors against the *same* provider before giving up.
///
/// This gives the CLI the same exponential-backoff retry behaviour that the
/// blocking [`seher::sdk::run_for_resolved`] path already has. Rate/usage
/// limits still surface as [`Outcome::Limit`] so the caller can switch
/// providers; timeouts and cancellations are not retried.
fn stream_with_http_retry(
    resolved: &ResolvedAgent,
    prompt: &str,
    system_prompt: Option<&str>,
    args: &Args,
    resume: Option<&str>,
    cancel: &CancelToken,
    logger: &Logger,
) -> Outcome {
    let mut attempt: u32 = 1;
    loop {
        let rx = dispatch_stream(resolved, prompt, system_prompt, args, resume, cancel);
        match drain_to_stdout(rx, args.timeout, cancel) {
            Outcome::Error(ref message)
                if resolved.retry.enabled
                    && attempt < resolved.retry.effective_max_attempts()
                    && resolved.retry.is_retryable_message(message) =>
            {
                let delay = resolved.retry.delay_for_attempt(attempt);
                logger.warn(&format!(
                    "Provider '{}' returned a transient API error (attempt {}/{}): {}; retrying in {}s...",
                    resolved.provider,
                    attempt,
                    resolved.retry.max_attempts,
                    message,
                    delay.as_secs()
                ));
                sleep_with_cancel(delay, cancel);
                if cancel.is_cancelled() {
                    return Outcome::Cancelled;
                }
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn resolve_once(
    rt: &tokio::runtime::Runtime,
    args: &Args,
    mode_key: &str,
    excluded: &[String],
    config: &Config,
) -> Result<ResolvedAgent, String> {
    let opts = ResolveOptions {
        mode_key: mode_key.to_string(),
        provider_filter: args.provider.clone(),
        config: Some(config.clone()),
        exclude_providers: excluded.to_vec(),
        quiet: args.quiet,
        ..Default::default()
    };

    // Limit determination is delegated to the external `codexbar` binary
    // (mirrors seher-ts), so no browser/cookie session is needed here.
    let mut probe = CodexBarProbe;
    rt.block_on(async move { resolve_agent(opts, &mut probe).await })
        .map_err(|e| e.to_string())
}

fn dispatch_stream(
    resolved: &ResolvedAgent,
    prompt: &str,
    system_prompt: Option<&str>,
    args: &Args,
    resume: Option<&str>,
    cancel: &CancelToken,
) -> std::sync::mpsc::Receiver<seher::sdk::StreamChunk> {
    // Resolve api_key: YAML config takes precedence, env var as fallback for
    // well-known providers (applies to the pi runner).
    let api_key = resolved
        .api
        .as_ref()
        .and_then(|a| a.key.clone())
        .or_else(|| env_api_key_for(Some(&resolved.provider)));

    stream_for_resolved(
        resolved,
        prompt.to_string(),
        RunAgentOptions {
            working_dir: args.cwd.as_deref().map(PathBuf::from),
            resume: resume.map(str::to_string),
            tools: Vec::new(),
            api_key,
            system_prompt: system_prompt.map(str::to_string),
            timeout_ms: args.timeout,
            cancel: cancel.clone(),
            on_retry: None,
        },
    )
}

fn env_api_key_for(provider: Option<&str>) -> Option<String> {
    let var = match provider {
        Some("anthropic") => "ANTHROPIC_API_KEY",
        Some("openai") => "OPENAI_API_KEY",
        _ => return None,
    };
    std::env::var(var).ok()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;
    use seher::sdk::ResolvedSkillsConfig;
    use std::cell::RefCell;

    fn make_resolved(provider: &str, model: &str) -> ResolvedAgent {
        ResolvedAgent {
            provider: provider.to_string(),
            model_id: model.to_string(),
            mode_key: "build".to_string(),
            sdk: "pi".to_string(),
            api: None,
            skills: ResolvedSkillsConfig::default(),
            retry: seher::sdk::RetryConfig::default(),
        }
    }

    fn silent_logger() -> Logger {
        Logger::new(true)
    }

    #[test]
    fn returns_done_on_first_success() {
        let logger = silent_logger();
        let resolver = |_excluded: &[String]| Ok(make_resolved("a", "anthropic/x"));
        let stream_runner = |_r: &ResolvedAgent| Outcome::Done("hi".to_string());
        let result = stream_with_retry(None, &logger, resolver, stream_runner).expect("done");
        assert_eq!(result, "hi");
    }

    #[test]
    fn retry_on_limit_excludes_resolved_provider_and_retries() {
        let logger = silent_logger();
        // Sequence of resolved providers: first "a" (limit), then "b" (done).
        let calls = RefCell::new(0u32);
        let resolver = |excluded: &[String]| {
            let n = *calls.borrow();
            *calls.borrow_mut() = n + 1;
            if n == 0 {
                // First call: excluded should be empty.
                assert!(excluded.is_empty(), "first resolve sees empty excluded");
                Ok(make_resolved("a", "anthropic/x"))
            } else {
                // Second call: excluded should contain "a" (the YAML provider name).
                assert_eq!(excluded, &["a".to_string()]);
                Ok(make_resolved("b", "openai/y"))
            }
        };
        let outcomes = RefCell::new(vec![Outcome::Limit, Outcome::Done("ok".to_string())]);
        let stream_runner = |_r: &ResolvedAgent| outcomes.borrow_mut().remove(0);
        let result = stream_with_retry(None, &logger, resolver, stream_runner).expect("done");
        assert_eq!(result, "ok");
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn duplicate_limit_does_not_grow_excluded() {
        // Resolver bug simulation: resolver keeps returning provider "a" even after
        // it's in `excluded`. The loop must not push "a" twice; it should terminate
        // when the resolver itself starts returning an error (no more candidates).
        let logger = silent_logger();
        let attempts = RefCell::new(0u32);
        let resolver = |excluded: &[String]| {
            let n = *attempts.borrow();
            *attempts.borrow_mut() = n + 1;
            if n >= 2 {
                return Err("no more candidates".to_string());
            }
            // Pretend resolver returns "a" regardless of excluded — checks dedup.
            let _ = excluded;
            Ok(make_resolved("a", "anthropic/x"))
        };
        let stream_runner = |_r: &ResolvedAgent| Outcome::Limit;
        let err =
            stream_with_retry(None, &logger, resolver, stream_runner).expect_err("should error");
        assert!(err.contains("no more candidates"), "got: {err}");
    }

    #[test]
    fn returns_error_on_stream_error() {
        let logger = silent_logger();
        let resolver = |_excluded: &[String]| Ok(make_resolved("a", "anthropic/x"));
        let stream_runner = |_r: &ResolvedAgent| Outcome::Error("boom".to_string());
        let err =
            stream_with_retry(None, &logger, resolver, stream_runner).expect_err("should error");
        assert_eq!(err, "boom");
    }

    #[test]
    fn returns_timeout_error_with_ms() {
        let logger = silent_logger();
        let resolver = |_excluded: &[String]| Ok(make_resolved("a", "anthropic/x"));
        let stream_runner = |_r: &ResolvedAgent| Outcome::Timeout;
        let err = stream_with_retry(Some(5000), &logger, resolver, stream_runner)
            .expect_err("should error");
        assert!(err.contains("5000"), "got: {err}");
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[test]
    fn resolver_error_propagates() {
        let logger = silent_logger();
        let resolver = |_excluded: &[String]| Err("config broken".to_string());
        let stream_runner = |_r: &ResolvedAgent| Outcome::Done(String::new());
        let err =
            stream_with_retry(None, &logger, resolver, stream_runner).expect_err("should error");
        assert_eq!(err, "config broken");
    }

    #[test]
    fn env_api_key_for_unknown_returns_none() {
        assert_eq!(env_api_key_for(None), None);
        assert_eq!(env_api_key_for(Some("cohere")), None);
        assert_eq!(env_api_key_for(Some("google")), None);
    }

    #[test]
    fn cancelled_outcome_does_not_retry_and_returns_err() {
        // Given: a stream_runner that immediately returns Cancelled
        let logger = silent_logger();
        let resolver = |_excluded: &[String]| Ok(make_resolved("a", "anthropic/x"));
        let stream_runner = |_r: &ResolvedAgent| Outcome::Cancelled;
        // When: stream_with_retry receives Outcome::Cancelled
        // Then: returns Err immediately without retrying (resolver called exactly once)
        let err =
            stream_with_retry(None, &logger, resolver, stream_runner).expect_err("should error");
        assert!(
            err.contains("cancel") || err.contains("interrupt"),
            "expected a cancellation error message, got: {err}"
        );
    }

    #[test]
    fn cancelled_outcome_does_not_add_to_excluded_and_does_not_retry() {
        // Given: a stream_runner that returns Cancelled on the first call
        // When: stream_with_retry receives Cancelled
        // Then: it must NOT treat it as a Limit and must NOT retry with a different provider
        let logger = silent_logger();
        let call_count = std::cell::RefCell::new(0u32);
        let resolver = |_excluded: &[String]| {
            *call_count.borrow_mut() += 1;
            Ok(make_resolved("a", "anthropic/x"))
        };
        let stream_runner = |_r: &ResolvedAgent| Outcome::Cancelled;
        let _ = stream_with_retry(None, &logger, resolver, stream_runner);
        assert_eq!(
            *call_count.borrow(),
            1,
            "resolver must be called exactly once — no retry on Cancelled"
        );
    }
}
