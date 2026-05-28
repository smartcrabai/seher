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

#[derive(Debug)]
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, String> {
        let raw = RawArgs::try_parse_from(std::iter::once("seher").chain(argv.iter().copied()))
            .map_err(|e| e.to_string())?;
        normalize(raw)
    }

    #[test]
    fn defaults_to_build_mode_with_no_args() {
        let a = parse(&[]).expect("ok");
        assert!(matches!(a.mode, Mode::Build));
        assert!(a.prompt_args.is_empty());
    }

    #[test]
    fn build_keyword_selects_build_mode_and_drops_token() {
        let a = parse(&["build", "do", "thing"]).expect("ok");
        assert!(matches!(a.mode, Mode::Build));
        assert_eq!(a.prompt_args, vec!["do".to_string(), "thing".to_string()]);
    }

    #[test]
    fn plan_keyword_selects_plan_mode_and_drops_token() {
        let a = parse(&["plan", "build", "a", "thing"]).expect("ok");
        assert!(matches!(a.mode, Mode::Plan));
        // "build" here is a prompt word, not a mode token (only the first one matters).
        assert_eq!(
            a.prompt_args,
            vec!["build".to_string(), "a".to_string(), "thing".to_string()],
        );
    }

    #[test]
    fn non_mode_first_token_defaults_to_build_keeping_all_words() {
        let a = parse(&["hello", "world"]).expect("ok");
        assert!(matches!(a.mode, Mode::Build));
        assert_eq!(
            a.prompt_args,
            vec!["hello".to_string(), "world".to_string()]
        );
    }

    #[test]
    fn timeout_zero_is_rejected() {
        let err = parse(&["-t", "0", "build", "x"]).expect_err("should reject");
        assert!(
            err.contains("Invalid --timeout") || err.contains("timeout"),
            "got: {err}"
        );
    }

    #[test]
    fn timeout_positive_value_is_accepted() {
        let a = parse(&["-t", "5000", "build", "x"]).expect("ok");
        assert_eq!(a.timeout, Some(5000));
    }

    #[test]
    fn provider_and_model_flags_propagate() {
        let a = parse(&["-p", "claude", "-m", "low", "build", "x"]).expect("ok");
        assert_eq!(a.provider.as_deref(), Some("claude"));
        assert_eq!(a.model.as_deref(), Some("low"));
    }

    #[test]
    fn unknown_browser_value_errors() {
        let err = parse(&["--browser", "lynx", "build", "x"]).expect_err("should reject");
        assert!(err.contains("--browser"), "got: {err}");
    }

    #[test]
    fn quiet_flag_sets_quiet() {
        let a = parse(&["-q", "build", "x"]).expect("ok");
        assert!(a.quiet);
    }
}
