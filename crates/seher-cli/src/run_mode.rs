//! Shared "resolve + stream prompt through pi" flow used by both build and plan modes.
//!
//! Implements the retry-on-limit loop: on a `LimitError`, the resolved YAML
//! provider name is added to `exclude_providers` and resolution is retried.

use seher::sdk::{
    BrowserSession, Config, CookieProbe, PiRunner, PiRunnerOptions, ResolveOptions, ResolvedAgent,
    TimeoutError, load_config, resolve_agent,
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
    prompt: String,
    args: &Args,
    mode_key: &str,
    system_prompt: Option<String>,
    logger: &Logger,
) -> Result<String, String> {
    // Load config + detect browser session once; reuse across retry attempts.
    let config: Config = load_config(args.config.as_deref()).map_err(|e| e.to_string())?;
    let browser_session = BrowserSession::detect(args.browser, args.profile.clone());

    let mut excluded: Vec<String> = Vec::new();
    loop {
        let resolved = resolve_once(rt, args, mode_key, &excluded, &config, &browser_session)?;
        logger.info(&format!(
            "Selected provider: {} (pi/{})",
            resolved.provider, resolved.model_id
        ));

        let runner = build_runner(&resolved, system_prompt.clone());
        let rx = runner.stream(prompt.clone());
        match drain_to_stdout(rx, args.timeout) {
            Outcome::Done(t) => return Ok(t),
            Outcome::Limit(_error) => {
                logger.warn(&format!(
                    "Provider '{}' hit API limit; retrying with next provider...",
                    resolved.provider
                ));
                // Exclude by the resolved YAML provider name (matches what
                // `resolve_agent` compares against), not by pi's provider id
                // which lives in a different namespace.
                if !excluded.contains(&resolved.provider) {
                    excluded.push(resolved.provider.clone());
                }
            }
            Outcome::Error(message) => return Err(message),
            Outcome::Timeout => {
                return Err(TimeoutError {
                    ms: args.timeout.unwrap_or(0),
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
    session: &BrowserSession,
) -> Result<ResolvedAgent, String> {
    let mut opts = ResolveOptions::default();
    opts.mode_key = mode_key.to_string();
    opts.provider_filter = args.provider.clone();
    opts.config = Some(config.clone());
    opts.exclude_providers = excluded.to_vec();
    opts.quiet = args.quiet;

    let mut probe = CookieProbe { session };
    rt.block_on(async move { resolve_agent(opts, &mut probe).await })
        .map_err(|e| e.to_string())
}

fn build_runner(resolved: &ResolvedAgent, system_prompt: Option<String>) -> PiRunner {
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
        tool_factory: None,
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
