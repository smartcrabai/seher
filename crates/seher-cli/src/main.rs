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

    let prompt = match prompt::resolve(&args.prompt_args) {
        Some(p) => p,
        None => {
            eprintln!("Empty prompt; nothing to do.");
            return 1;
        }
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

    let outcome = dispatch(&rt, prompt, &args, &logger);
    match outcome {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

fn dispatch(
    rt: &tokio::runtime::Runtime,
    prompt: String,
    args: &Args,
    logger: &logger::Logger,
) -> Result<(), String> {
    match args.mode {
        Mode::Plan => mode_plan::run(rt, prompt, args, logger),
        Mode::Build => mode_build::run(rt, prompt, args, logger),
    }
}
