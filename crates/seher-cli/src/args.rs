use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use seher::BrowserType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Build,
}

#[derive(Parser, Debug)]
#[command(
    name = "seher",
    about = "Pick the highest-priority coding agent and run a plan/build prompt",
    version,
    disable_help_subcommand = true
)]
pub struct RawArgs {
    /// Force a specific provider key
    #[arg(short = 'p', long)]
    pub provider: Option<String>,

    /// Mode/model key override (e.g. `low`, `build`)
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// YAML config path
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,

    /// Per-run timeout in milliseconds
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,

    /// Suppress informational output
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Browser to read cookies from
    #[arg(long)]
    pub browser: Option<String>,

    /// Browser profile name
    #[arg(long)]
    pub profile: Option<String>,

    /// Optional `plan`/`build` followed by prompt text
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub trailing: Vec<String>,
}

pub struct Args {
    pub mode: Mode,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub config: Option<PathBuf>,
    pub timeout: Option<u64>,
    pub quiet: bool,
    pub browser: Option<BrowserType>,
    pub profile: Option<String>,
    pub prompt_args: Vec<String>,
}

/// Convert clap-parsed `RawArgs` into the normalized [`Args`].
pub fn normalize(raw: RawArgs) -> Result<Args, String> {
    let (mode, prompt_args) = match raw.trailing.first().map(String::as_str) {
        Some("plan") => (Mode::Plan, raw.trailing[1..].to_vec()),
        Some("build") => (Mode::Build, raw.trailing[1..].to_vec()),
        _ => (Mode::Build, raw.trailing),
    };

    let browser = match raw.browser.as_deref() {
        None => None,
        Some(s) => {
            Some(BrowserType::from_str(s).map_err(|e| format!("invalid --browser '{s}': {e}"))?)
        }
    };

    if let Some(t) = raw.timeout
        && t == 0
    {
        return Err(format!(
            "Invalid --timeout value '{t}': expected a positive integer (ms)"
        ));
    }

    Ok(Args {
        mode,
        provider: raw.provider,
        model: raw.model,
        config: raw.config,
        timeout: raw.timeout,
        quiet: raw.quiet,
        browser,
        profile: raw.profile,
        prompt_args,
    })
}
