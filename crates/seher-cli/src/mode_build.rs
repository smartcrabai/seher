use crate::args::Args;
use crate::logger::Logger;
use crate::run_mode::resolve_and_stream;

/// Run a build-mode prompt: resolve the highest-priority non-limited provider for
/// the build mode key and stream the prompt through it.
///
/// # Errors
///
/// Stringified errors from resolution, timeout, or non-limit pi errors.
pub fn run(
    rt: &tokio::runtime::Runtime,
    prompt: String,
    args: &Args,
    logger: &Logger,
) -> Result<(), String> {
    let mode_key = args.model.clone().unwrap_or_else(|| "build".to_string());
    resolve_and_stream(rt, prompt, args, &mode_key, None, logger)?;
    Ok(())
}
