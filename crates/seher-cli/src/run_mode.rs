//! Shared "resolve + stream prompt through pi" flow used by both build and plan modes.
//!
//! Implements the retry-on-limit loop: on a `LimitError`, the resolved YAML
//! provider name is added to `exclude_providers` and resolution is retried.

use seher::claude_terminal::{new_sdk_with_defaults, stream_via_thread};
use seher::sdk::{
    CodexBarProbe, Config, PiRunner, PiRunnerOptions, ResolveOptions, ResolvedAgent, TimeoutError,
    load_config, resolve_agent, unsupported_sdk_providers,
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

    // One-time warning for non-executable providers (e.g. `sdk: claude` from
    // seher-ts configs). They are silently filtered out of candidates by the
    // resolver; we surface the skip here so the user knows why.
    for (provider, sdk) in unsupported_sdk_providers(&config) {
        logger.warn(&format!(
            "Skipping provider '{provider}' (sdk='{sdk}'): not supported by this build (supported: 'pi', 'claude-terminal')"
        ));
    }

    let resolver = |excluded: &[String]| -> Result<ResolvedAgent, String> {
        resolve_once(rt, args, mode_key, excluded, &config)
    };
    let stream_runner = |resolved: &ResolvedAgent| -> Outcome {
        let rx = dispatch_stream(resolved, prompt, system_prompt, args);
        drain_to_stdout(rx, args.timeout)
    };
    stream_with_retry(args.timeout, logger, resolver, stream_runner)
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
) -> std::sync::mpsc::Receiver<seher::sdk::StreamChunk> {
    if resolved.sdk == "claude-terminal" {
        let model = Some(resolved.model_id.clone()).filter(|s| !s.is_empty());
        let sdk = new_sdk_with_defaults(
            None,
            None,
            model,
            system_prompt.map(str::to_string),
            args.timeout,
            None,
        );
        stream_via_thread(sdk, prompt.to_string(), resolved.provider.clone())
    } else {
        let runner = build_pi_runner(resolved, system_prompt.map(str::to_string));
        runner.stream(prompt.to_string())
    }
}

fn build_pi_runner(resolved: &ResolvedAgent, system_prompt: Option<String>) -> PiRunner {
    let (provider, model) = parse_provider_model(&resolved.model_id);
    let api_key = resolved
        .api
        .as_ref()
        .and_then(|a| a.key.clone())
        .or_else(|| env_api_key_for(provider.as_deref()));
    PiRunner::new(PiRunnerOptions {
        provider,
        model,
        api_key,
        system_prompt,
    })
}

fn parse_provider_model(model_id: &str) -> (Option<String>, Option<String>) {
    if let Some((p, m)) = model_id.split_once('/') {
        (Some(p.to_string()), Some(m.to_string()))
    } else {
        (None, Some(model_id.to_string()))
    }
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
    fn parse_provider_model_with_slash() {
        let (p, m) = parse_provider_model("anthropic/claude-sonnet");
        assert_eq!(p.as_deref(), Some("anthropic"));
        assert_eq!(m.as_deref(), Some("claude-sonnet"));
    }

    #[test]
    fn parse_provider_model_without_slash() {
        let (p, m) = parse_provider_model("just-a-model");
        assert_eq!(p, None);
        assert_eq!(m.as_deref(), Some("just-a-model"));
    }

    #[test]
    fn env_api_key_for_unknown_returns_none() {
        assert_eq!(env_api_key_for(None), None);
        assert_eq!(env_api_key_for(Some("cohere")), None);
        assert_eq!(env_api_key_for(Some("google")), None);
    }
}
