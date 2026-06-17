mod args;
mod logger;
mod mode_build;
mod mode_plan;
mod prompt;
mod run_mode;
mod stream;

use clap::Parser;

use crate::args::{Args, Mode, RawArgs, normalize};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let raw = match RawArgs::try_parse() {
        Ok(r) => r,
        Err(e) => {
            let _ = e.print();
            return match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
        }
    };
    let args = match normalize(raw) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    if args.show_resolution {
        return match show_resolution(&args) {
            Ok(()) => 0,
            Err(msg) => {
                eprintln!("{msg}");
                1
            }
        };
    }

    let Some(prompt) = prompt::resolve(&args.prompt_tokens) else {
        eprintln!("Empty prompt; nothing to do.");
        return 1;
    };

    let logger = logger::Logger::new(args.quiet);

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to build tokio runtime: {e}");
            return 1;
        }
    };

    let outcome = dispatch(&rt, &prompt, &args, &logger);
    match outcome {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

fn show_resolution(args: &Args) -> Result<(), String> {
    use seher::sdk::{
        CodexBarProbe, ResolveOptions, build_candidates, load_config, resolve_agent,
        unsupported_sdk_providers,
    };

    let config = load_config(args.config.as_deref()).map_err(|e| e.to_string())?;

    let mode_key = args.model.clone().unwrap_or_else(|| match args.mode {
        Mode::Plan => "plan".to_string(),
        Mode::Build => "build".to_string(),
    });

    // Show all candidates
    let candidates = build_candidates(&config, &mode_key, args.provider.as_deref(), &[]);
    if candidates.is_empty() {
        eprintln!("No candidates for mode key '{mode_key}'");
    } else {
        eprintln!("Candidates (mode={mode_key}):");
        for (i, c) in candidates.iter().enumerate() {
            eprintln!(
                "  {i}. {} (sdk={}, model={}, priority={})",
                c.resolved.provider, c.resolved.sdk, c.resolved.model_id, c.priority
            );
        }
        eprintln!();
    }

    // Show skipped providers
    for (provider, sdk) in unsupported_sdk_providers(&config) {
        eprintln!("Skipped: {provider} (sdk={sdk}, not supported)");
    }

    // Resolve with codexbar probe
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to build tokio runtime: {e}"))?;

    let opts = ResolveOptions {
        mode_key: mode_key.clone(),
        provider_filter: args.provider.clone(),
        config: Some(config),
        no_wait: true,
        quiet: args.quiet,
        ..Default::default()
    };

    let mut probe = CodexBarProbe;
    match rt.block_on(resolve_agent(opts, &mut probe)) {
        Ok(agent) => {
            println!(
                "{}",
                serde_json::json!({
                    "provider": agent.provider,
                    "model": agent.model_id,
                    "sdk": agent.sdk,
                    "mode": agent.mode_key,
                })
            );
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn dispatch(
    rt: &tokio::runtime::Runtime,
    prompt: &str,
    args: &Args,
    logger: &logger::Logger,
) -> Result<(), String> {
    match args.mode {
        Mode::Plan => mode_plan::run(rt, prompt, args, logger),
        Mode::Build => mode_build::run(rt, prompt, args, logger),
    }
}
