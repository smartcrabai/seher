use std::path::PathBuf;

use clap::Parser;
use seher::sdk::EffortLevel;

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

    /// Show which provider/model/SDK would be selected and exit (no prompt required)
    #[arg(long)]
    pub show_resolution: bool,

    /// Working directory for the agent. Multi-turn sessions are bound to it.
    #[arg(long)]
    pub cwd: Option<String>,

    /// Resume a prior session by id (printed as `session: <id>` on a previous run).
    /// Pass the same `--cwd` used to create it.
    #[arg(short = 'r', long)]
    pub resume: Option<String>,

    /// Effort level for the current session (low, medium, high, xhigh, max)
    #[arg(long, value_parser = clap::value_parser!(EffortLevel))]
    pub effort: Option<EffortLevel>,

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
    pub show_resolution: bool,
    /// Absolute, canonicalized working directory (when `--cwd` was given).
    pub cwd: Option<String>,
    /// Session id to resume, if any.
    pub resume: Option<String>,
    pub effort: Option<EffortLevel>,
    pub prompt_tokens: Vec<String>,
}

/// Convert clap-parsed `RawArgs` into the normalized [`Args`].
pub fn normalize(raw: RawArgs) -> Result<Args, String> {
    let (mode, prompt_tokens) = match raw.trailing.first().map(String::as_str) {
        Some("plan") => (Mode::Plan, raw.trailing[1..].to_vec()),
        Some("build") => (Mode::Build, raw.trailing[1..].to_vec()),
        _ => (Mode::Build, raw.trailing),
    };

    if let Some(t) = raw.timeout
        && t == 0
    {
        return Err(format!(
            "Invalid --timeout value '{t}': expected a positive integer (ms)"
        ));
    }

    // Session ids are uuids (or claude-generated ids of the same shape); they are used
    // to build file paths, so reject anything with path separators or other junk.
    if let Some(r) = &raw.resume
        && (r.is_empty()
            || !r
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return Err(format!(
            "Invalid --resume value '{r}': expected a session id (alphanumeric, '-', '_')"
        ));
    }

    // Canonicalize --cwd to an absolute path up front, so the session<->cwd binding is
    // stable across turns (a relative `.` and its absolute form must encode identically
    // for resume to locate the transcript). Errors if the directory does not exist.
    let cwd = match raw.cwd {
        Some(c) => Some(
            std::fs::canonicalize(&c)
                .map_err(|e| format!("Invalid --cwd '{c}': {e}"))
                .and_then(|p| {
                    if p.is_dir() {
                        Ok(p.to_string_lossy().into_owned())
                    } else {
                        Err(format!("Invalid --cwd '{c}': not a directory"))
                    }
                })?,
        ),
        None => None,
    };

    Ok(Args {
        mode,
        provider: raw.provider,
        model: raw.model,
        config: raw.config,
        timeout: raw.timeout,
        quiet: raw.quiet,
        show_resolution: raw.show_resolution,
        cwd,
        resume: raw.resume,
        effort: raw.effort,
        prompt_tokens,
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
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
        assert!(a.prompt_tokens.is_empty());
    }

    #[test]
    fn build_keyword_selects_build_mode_and_drops_token() {
        let a = parse(&["build", "do", "thing"]).expect("ok");
        assert!(matches!(a.mode, Mode::Build));
        assert_eq!(a.prompt_tokens, vec!["do".to_string(), "thing".to_string()]);
    }

    #[test]
    fn plan_keyword_selects_plan_mode_and_drops_token() {
        let a = parse(&["plan", "build", "a", "thing"]).expect("ok");
        assert!(matches!(a.mode, Mode::Plan));
        // "build" here is a prompt word, not a mode token (only the first one matters).
        assert_eq!(
            a.prompt_tokens,
            vec!["build".to_string(), "a".to_string(), "thing".to_string()],
        );
    }

    #[test]
    fn non_mode_first_token_defaults_to_build_keeping_all_words() {
        let a = parse(&["hello", "world"]).expect("ok");
        assert!(matches!(a.mode, Mode::Build));
        assert_eq!(
            a.prompt_tokens,
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
    fn quiet_flag_sets_quiet() {
        let a = parse(&["-q", "build", "x"]).expect("ok");
        assert!(a.quiet);
    }

    #[test]
    fn effort_flag_propagates() {
        let a = parse(&["--effort", "high", "build", "x"]).expect("ok");
        assert_eq!(a.effort, Some(EffortLevel::High));
    }

    #[test]
    fn effort_defaults_to_none() {
        let a = parse(&["build", "x"]).expect("ok");
        assert_eq!(a.effort, None);
    }

    #[test]
    fn effort_rejects_invalid_value() {
        let err = parse(&["--effort", "nonsense", "build", "x"]).expect_err("should reject");
        assert!(err.contains("effort"), "got: {err}");
    }

    #[test]
    fn resume_accepts_uuid_like_ids() {
        let a = parse(&["-r", "963f3c95-78ba-472a-8adf-a5218af2d135", "build", "x"]).expect("ok");
        assert_eq!(
            a.resume.as_deref(),
            Some("963f3c95-78ba-472a-8adf-a5218af2d135")
        );
    }

    #[test]
    fn resume_rejects_path_separators() {
        let err = parse(&["-r", "../../../etc/passwd", "build", "x"]).expect_err("should reject");
        assert!(err.contains("Invalid --resume"), "got: {err}");
        let err = parse(&["-r", "a/b", "build", "x"]).expect_err("should reject");
        assert!(err.contains("Invalid --resume"), "got: {err}");
    }

    #[test]
    fn cwd_must_exist() {
        let err =
            parse(&["--cwd", "/nonexistent-dir-xyz", "build", "x"]).expect_err("should reject");
        assert!(err.contains("Invalid --cwd"), "got: {err}");
    }

    #[test]
    fn cwd_is_canonicalized() {
        let a = parse(&["--cwd", "/tmp", "build", "x"]).expect("ok");
        // macOS: /tmp is a symlink to /private/tmp -- canonicalize resolves it.
        let expected = std::fs::canonicalize("/tmp")
            .expect("canonicalize /tmp")
            .to_string_lossy()
            .into_owned();
        assert_eq!(a.cwd.as_deref(), Some(expected.as_str()));
    }
}
