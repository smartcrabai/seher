use chrono::{DateTime, Local, Utc};
use clap::Parser;
use seher::{Agent, AgentLimit, AgentStatus, BrowserDetector, BrowserType, CookieReader, Settings};
use std::path::PathBuf;
use std::str::FromStr;
use zzsleep::sleep_until;

#[derive(Parser)]
#[command(
    name = "seher",
    version,
    about = "CLI tool for Claude.ai, Codex, and Copilot rate limit monitoring"
)]
pub struct Args {
    /// Browser to use (chrome, edge, brave, firefox, safari, etc.)
    #[arg(long, short)]
    pub browser: Option<String>,

    /// Browser profile name (e.g. "Profile 1", "default-release")
    #[arg(long)]
    pub profile: Option<String>,

    /// Additional arguments to pass to the agent command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub agent_args: Vec<String>,

    /// Model level to use (e.g. "high", "low"), resolved via agent's models map
    #[arg(long, short)]
    pub model: Option<String>,

    /// Suppress informational output (usage, sleep progress, etc.)
    #[arg(long, short)]
    pub quiet: bool,

    /// Output provider usage as JSON and exit
    #[arg(long, short = 'j')]
    pub json: bool,

    /// Path to settings file
    #[arg(long, short = 'C')]
    pub config: Option<PathBuf>,
}

/// Normalized result of executing a child agent process.
#[derive(Debug, PartialEq)]
enum ChildExitKind {
    /// Process exited with status code 0.
    Success,
    /// Process exited with a non-zero status (or unknown code).
    Failure { code: Option<i32> },
    /// Process was terminated by a signal (Unix only).
    SignalTerminated,
    /// Process could not be spawned (IO error before execution).
    SpawnError,
}

impl From<std::io::Result<std::process::ExitStatus>> for ChildExitKind {
    fn from(result: std::io::Result<std::process::ExitStatus>) -> Self {
        match result {
            Err(_) => ChildExitKind::SpawnError,
            Ok(status) if status.success() => ChildExitKind::Success,
            Ok(status) if status.code().is_none() => ChildExitKind::SignalTerminated,
            Ok(status) => ChildExitKind::Failure {
                code: status.code(),
            },
        }
    }
}

/// Preserved invocation state that can be reused across auto-rerun attempts.
struct InvocationInput {
    /// Raw trailing args as received from the CLI, before agent-specific mapping.
    pub raw_agent_args: Vec<String>,
    /// Prompt obtained from the editor on the first attempt; reused on rerun.
    pub cached_prompt: Option<String>,
}

/// Return `true` if an auto-rerun should be triggered.
///
/// Rules:
/// - Only provider-aware agents (domain != None) trigger auto-rerun.
/// - Only `Failure` exits trigger auto-rerun (not Success, SpawnError, or SignalTerminated).
fn should_auto_rerun(exit_kind: &ChildExitKind, agent_is_provider_aware: bool) -> bool {
    matches!(exit_kind, ChildExitKind::Failure { .. }) && agent_is_provider_aware
}

pub async fn run(args: Args) {
    let settings = match Settings::load(args.config.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load settings: {}", e);
            return;
        }
    };

    let detector = BrowserDetector::new();
    let browsers = detector.detect_browsers();

    if browsers.is_empty() {
        eprintln!("No browsers found");
        return;
    }

    let mut agents: Vec<Agent> = Vec::new();

    for config in &settings.agents {
        let domain = config.resolve_domain();

        let cookies = match domain {
            Some(d) => {
                match get_cookies_for_domain(&detector, &browsers, &args.browser, &args.profile, d)
                {
                    Some(c) => c,
                    None => {
                        if !args.quiet {
                            eprintln!("No cookies found for {} (domain: {})", config.command, d);
                        }
                        continue;
                    }
                }
            }
            None => vec![],
        };

        agents.push(Agent::new(config.clone(), cookies));
    }

    if agents.is_empty() {
        eprintln!("No agents with valid cookies found");
        return;
    }

    if args.json {
        let mut statuses: Vec<AgentStatus> = Vec::new();
        for agent in &agents {
            match agent.fetch_status().await {
                Ok(status) => statuses.push(status),
                Err(e) => eprintln!("Failed to fetch status for {}: {}", agent.command(), e),
            }
        }
        match serde_json::to_string_pretty(&statuses) {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Failed to serialize status: {}", e),
        }
        return;
    }

    type LimitResult = (usize, Result<seher::AgentLimit, Box<dyn std::error::Error>>);
    let mut limit_results: Vec<LimitResult> = Vec::new();

    for (i, agent) in agents.iter().enumerate() {
        if !args.quiet {
            println!(
                "Checking limit for {} {}...",
                agent.command(),
                agent.resolved_args(args.model.as_deref()).join(" ")
            );
        }

        let result = agent.check_limit().await;
        limit_results.push((i, result));
    }

    let mut available_indices: Vec<usize> = Vec::new();
    let mut limited_indices: Vec<(usize, Option<DateTime<Utc>>)> = Vec::new();

    for (i, result) in &limit_results {
        match result {
            Ok(AgentLimit::NotLimited) => {
                available_indices.push(*i);
            }
            Ok(AgentLimit::Limited { reset_time }) => {
                limited_indices.push((*i, *reset_time));
            }
            Err(e) => {
                if !args.quiet {
                    eprintln!("Failed to check limit for agent {}: {}", i, e);
                }
            }
        }
    }

    if let Some(model_key) = args.model.as_deref() {
        if !agents.iter().any(|a| a.has_model(model_key)) {
            eprintln!("No agents found with model '{}'", model_key);
            return;
        }

        available_indices.retain(|&i| agents[i].has_model(model_key));
        limited_indices.retain(|(i, _)| agents[*i].has_model(model_key));
    }

    // Prioritize provider-aware agents (with domain) and put fallback agents last
    available_indices.sort_by_key(|&i| agents[i].config.resolve_domain().is_none());

    let mut input = InvocationInput {
        raw_agent_args: args.agent_args,
        cached_prompt: None,
    };

    if !available_indices.is_empty() {
        let selected_index = available_indices[0];
        if !args.quiet {
            println!(
                "Agent {} is available (not limited)",
                agents[selected_index].command()
            );
        }
        execute_with_auto_rerun(
            &agents,
            selected_index,
            &mut input,
            args.model.as_deref(),
            args.quiet,
        )
        .await;
        return;
    }

    if !limited_indices.is_empty() {
        let mut earliest: Option<(usize, DateTime<Utc>)> = None;
        for (i, reset_time) in &limited_indices {
            if let Some(rt) = reset_time
                && (earliest.is_none() || *rt < earliest.unwrap().1)
            {
                earliest = Some((*i, *rt));
            }
        }

        if let Some((idx, rt)) = earliest {
            if !args.quiet {
                println!(
                    "All agents limited. Waiting for {} ({} seconds)...",
                    rt.format("%Y-%m-%d %H:%M:%S UTC"),
                    (rt - Utc::now()).num_seconds()
                );
            }
            sleep_until_reset(rt, args.quiet).await;
            execute_with_auto_rerun(&agents, idx, &mut input, args.model.as_deref(), args.quiet)
                .await;
            return;
        } else if !args.quiet {
            println!("All agents limited, no reset time available");
        }
    }

    eprintln!("No available agents");
}

fn get_cookies_for_domain(
    detector: &BrowserDetector,
    browsers: &[BrowserType],
    browser_arg: &Option<String>,
    profile_arg: &Option<String>,
    domain: &str,
) -> Option<Vec<seher::Cookie>> {
    if let Some(browser_name) = browser_arg {
        let browser_type = BrowserType::from_str(browser_name).ok()?;
        if !browsers.contains(&browser_type) {
            return None;
        }

        if let Some(profile_name) = profile_arg {
            let prof = detector.get_profile(browser_type, Some(profile_name))?;
            let cookies = CookieReader::read_cookies(&prof, domain).ok()?;
            if cookies.iter().any(|c| has_session_cookie(domain, c)) {
                return Some(cookies);
            }
            return None;
        }

        for prof in detector.list_profiles(browser_type) {
            if let Ok(cookies) = CookieReader::read_cookies(&prof, domain)
                && cookies.iter().any(|c| has_session_cookie(domain, c))
            {
                return Some(cookies);
            }
        }
        return None;
    }

    for browser in browsers {
        if !browser.is_chromium_based() {
            continue;
        }
        for prof in detector.list_profiles(*browser) {
            if let Ok(cookies) = CookieReader::read_cookies(&prof, domain)
                && cookies.iter().any(|c| has_session_cookie(domain, c))
            {
                return Some(cookies);
            }
        }
    }

    None
}

fn has_session_cookie(domain: &str, cookie: &seher::Cookie) -> bool {
    match domain {
        "claude.ai" => cookie.name == "sessionKey",
        "chatgpt.com" => cookie.name.starts_with("__Secure-next-auth.session-token"),
        "github.com" => {
            cookie.name == "user_session" || cookie.name == "__Host-user_session_same_site"
        }
        _ => false,
    }
}

async fn prompt_from_editor() -> std::result::Result<String, Box<dyn std::error::Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    std::process::Command::new(&editor)
        .arg(tmp.path())
        .status()?;
    Ok(std::fs::read_to_string(tmp.path())?.trim().to_string())
}

async fn execute_with_auto_rerun(
    agents: &[Agent],
    idx: usize,
    input: &mut InvocationInput,
    model: Option<&str>,
    quiet: bool,
) {
    let exit_kind = execute_agent(agents, idx, input, model, quiet).await;
    let provider_aware = agents[idx].config.resolve_domain().is_some();
    if should_auto_rerun(&exit_kind, provider_aware) {
        if !quiet {
            eprintln!("Agent failed, retrying...");
        }
        execute_agent(agents, idx, input, model, quiet).await;
    }
}

async fn execute_agent(
    agents: &[Agent],
    selected_index: usize,
    input: &mut InvocationInput,
    model: Option<&str>,
    quiet: bool,
) -> ChildExitKind {
    let selected_agent = &agents[selected_index];
    let mut final_args = selected_agent.mapped_args(&input.raw_agent_args);

    if input.raw_agent_args.is_empty() && !quiet {
        if input.cached_prompt.is_none() {
            match prompt_from_editor().await {
                Ok(prompt) => input.cached_prompt = Some(prompt),
                Err(e) => {
                    eprintln!("Editor error: {}", e);
                    // SpawnError prevents auto-rerun, which is correct — the agent was never started.
                    return ChildExitKind::SpawnError;
                }
            }
        }
        // cached_prompt is guaranteed Some at this point
        let p = input
            .cached_prompt
            .as_deref()
            .expect("cached_prompt was just set");
        if !p.is_empty() {
            final_args.push(p.to_string());
        }
    }

    let resolved = selected_agent.resolved_args(model);
    if !quiet {
        println!(
            "Executing: {} {}",
            selected_agent.command(),
            resolved
                .iter()
                .chain(final_args.iter())
                .map(|s: &String| s.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    selected_agent.execute(&resolved, &final_args).into()
}

async fn sleep_until_reset(reset_time: DateTime<Utc>, quiet: bool) {
    let now = Utc::now();
    if reset_time <= now {
        if !quiet {
            println!("\nReset time has already passed, no sleep needed.");
        }
        return;
    }

    let total_secs = (reset_time - now).num_seconds().max(0) as u64;
    if !quiet {
        println!(
            "\nSleeping until {} ({} seconds)...",
            reset_time.format("%Y-%m-%d %H:%M:%S UTC"),
            total_secs
        );
    }

    let local_reset_time = reset_time.with_timezone(&Local);
    sleep_until(local_reset_time, quiet).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cookie(name: &str) -> seher::Cookie {
        seher::Cookie {
            name: name.to_string(),
            value: "value".to_string(),
            domain: ".chatgpt.com".to_string(),
            path: "/".to_string(),
            expires_utc: 0,
            is_secure: true,
            is_httponly: true,
            same_site: 0,
        }
    }

    // -----------------------------------------------------------------------
    // should_auto_rerun
    // -----------------------------------------------------------------------

    #[test]
    fn should_auto_rerun_returns_false_for_success() {
        assert!(!should_auto_rerun(&ChildExitKind::Success, true));
    }

    #[test]
    fn should_auto_rerun_returns_false_for_spawn_error() {
        assert!(!should_auto_rerun(&ChildExitKind::SpawnError, true));
    }

    #[test]
    fn should_auto_rerun_returns_false_for_signal_terminated() {
        assert!(!should_auto_rerun(&ChildExitKind::SignalTerminated, true));
    }

    #[test]
    fn should_auto_rerun_returns_false_for_fallback_agent_failure() {
        assert!(!should_auto_rerun(
            &ChildExitKind::Failure { code: Some(1) },
            false,
        ));
    }

    #[test]
    fn should_auto_rerun_returns_true_for_provider_aware_failure() {
        assert!(should_auto_rerun(
            &ChildExitKind::Failure { code: Some(1) },
            true,
        ));
    }

    #[test]
    fn should_auto_rerun_returns_true_for_provider_aware_failure_with_no_exit_code() {
        assert!(should_auto_rerun(
            &ChildExitKind::Failure { code: None },
            true,
        ));
    }

    #[test]
    fn has_session_cookie_recognizes_codex_session_cookie_fragments() {
        assert!(has_session_cookie(
            "chatgpt.com",
            &sample_cookie("__Secure-next-auth.session-token.0"),
        ));
        assert!(has_session_cookie(
            "chatgpt.com",
            &sample_cookie("__Secure-next-auth.session-token"),
        ));
        assert!(!has_session_cookie(
            "chatgpt.com",
            &sample_cookie("cf_clearance"),
        ));
    }

    // -----------------------------------------------------------------------
    // ChildExitKind::from (From<io::Result<ExitStatus>>)
    // -----------------------------------------------------------------------

    #[test]
    fn child_exit_kind_from_returns_spawn_error_on_io_error() {
        // Given: an IO error representing a failed spawn (binary not found, permission denied, etc.)
        // When:  converting to ChildExitKind
        // Then:  returns SpawnError
        let result: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert_eq!(ChildExitKind::from(result), ChildExitKind::SpawnError);
    }

    #[test]
    #[cfg(unix)]
    fn child_exit_kind_from_returns_success_for_zero_exit() {
        // Given: a process that exits cleanly with code 0
        // When:  converting to ChildExitKind
        // Then:  returns Success
        let status = std::process::Command::new("true")
            .status()
            .expect("`true` command must exist on Unix");
        assert_eq!(ChildExitKind::from(Ok(status)), ChildExitKind::Success);
    }

    #[test]
    #[cfg(unix)]
    fn child_exit_kind_from_returns_failure_for_nonzero_exit() {
        // Given: a process that exits with code 1
        // When:  converting to ChildExitKind
        // Then:  returns Failure with code Some(1)
        let status = std::process::Command::new("false")
            .status()
            .expect("`false` command must exist on Unix");
        assert_eq!(
            ChildExitKind::from(Ok(status)),
            ChildExitKind::Failure { code: Some(1) },
        );
    }

    #[test]
    #[cfg(unix)]
    fn child_exit_kind_from_returns_signal_terminated_for_signal_killed_process() {
        // Given: a process terminated by SIGKILL (Unix signal)
        // When:  converting to ChildExitKind
        // Then:  returns SignalTerminated (signal termination ≠ voluntary non-zero exit)
        use std::os::unix::process::ExitStatusExt;
        // Spawn a long-running process and kill it with SIGKILL
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("`sleep` command must exist on Unix");
        child.kill().expect("kill must succeed");
        let status = child.wait().expect("wait must succeed");
        // SIGKILL (signal 9) → signal() returns Some(9), code() returns None
        assert!(status.signal().is_some());
        assert_eq!(
            ChildExitKind::from(Ok(status)),
            ChildExitKind::SignalTerminated
        );
    }
}
