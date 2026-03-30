use chrono::{DateTime, Local, Utc};
use clap::Parser;
use seher::{
    Agent, AgentConfig, AgentLimit, AgentStatus, BrowserDetector, BrowserType, CodexClient,
    CookieReader, Settings,
};
use std::cmp::Reverse;
use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use zzsleep::sleep_until;

#[derive(Parser)]
#[command(
    name = "seher",
    version,
    about = "CLI tool for Claude.ai, Codex, and Copilot rate limit monitoring"
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "CLI flags map naturally to bools"
)]
pub struct Args {
    /// Browser to use (chrome, edge, brave, firefox, safari, etc.)
    #[arg(long, short)]
    pub browser: Option<String>,

    /// Browser profile name (e.g. "Profile 1", "default-release")
    #[arg(long)]
    pub profile: Option<String>,

    /// Filter agents by command name
    #[arg(long)]
    pub command: Option<String>,

    /// Filter agents by provider name (resolved)
    #[arg(long)]
    pub provider: Option<String>,

    /// Additional arguments to pass to the agent command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub extra: Vec<String>,

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

    /// Show priority order for each model level and exit
    #[arg(long)]
    pub priority: bool,

    /// Open the web-based config editor and exit when the server stops
    #[arg(long)]
    pub gui_config: bool,
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

/// Tri-state representing how a user-supplied prompt has been resolved from stdin.
#[derive(Debug)]
enum PromptState {
    /// stdin was a TTY; the prompt has not been resolved (editor fallback may apply).
    Unresolved,
    /// stdin was non-TTY and contained a non-empty prompt after trimming.
    Resolved(String),
    /// stdin was non-TTY but was empty or whitespace-only; no prompt to inject.
    Empty,
}

/// Preserved invocation state that can be reused across auto-rerun attempts.
struct InvocationInput {
    /// Raw trailing args as received from the CLI, before agent-specific mapping.
    pub raw_agent_args: Vec<String>,
    /// Prompt obtained from the editor on the first attempt; reused on rerun.
    pub cached_prompt: Option<String>,
    /// Prompt resolved from stdin before the first execution attempt (tri-state).
    pub stdin_prompt: PromptState,
}

/// Return `true` if an auto-rerun should be triggered.
///
/// Rules:
/// - Only provider-aware agents (provider != None) trigger auto-rerun.
/// - Only `Failure` exits trigger auto-rerun (not `Success`, `SpawnError`, or `SignalTerminated`).
fn should_auto_rerun(exit_kind: &ChildExitKind, agent_is_provider_aware: bool) -> bool {
    matches!(exit_kind, ChildExitKind::Failure { .. }) && agent_is_provider_aware
}

fn select_best_available_agent(
    settings: &Settings,
    agents: &[Agent],
    available_indices: &[usize],
    model: Option<&str>,
) -> Option<usize> {
    available_indices
        .iter()
        .copied()
        .min_by_key(|&i| (Reverse(settings.priority_for(&agents[i].config, model)), i))
}

pub async fn run(args: Args) {
    let settings = match Settings::load(args.config.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load settings: {e}");
            return;
        }
    };

    if args.priority {
        print_priority(&settings);
        return;
    }

    if args.gui_config {
        if let Err(e) = seher::web::serve(settings, args.config).await {
            eprintln!("Config editor error: {e}");
        }
        return;
    }

    let detector = BrowserDetector::new();
    let browsers = detector.detect_browsers();

    if browsers.is_empty() {
        eprintln!("No browsers found");
        return;
    }

    let agents = build_agents(&settings, &detector, &browsers, &args).await;

    if agents.is_empty() {
        eprintln!("No agents with valid cookies found");
        return;
    }

    let agents = filter_agents(agents, args.command.as_deref(), args.provider.as_deref());

    if agents.is_empty() {
        eprintln!("No agents match the specified filters");
        return;
    }

    if args.json {
        print_json_status(&agents).await;
        return;
    }

    run_with_limit_check(&settings, agents, &args).await;
}

fn filter_agents(
    mut agents: Vec<Agent>,
    command: Option<&str>,
    provider: Option<&str>,
) -> Vec<Agent> {
    if let Some(cmd) = command {
        agents.retain(|a| a.command() == cmd);
    }
    if let Some(p) = provider {
        agents.retain(|a| a.config.resolve_provider() == Some(p));
    }
    agents
}

async fn build_agents(
    settings: &Settings,
    detector: &BrowserDetector,
    browsers: &[BrowserType],
    args: &Args,
) -> Vec<Agent> {
    let mut agents: Vec<Agent> = Vec::new();
    for config in &settings.agents {
        let domain = config.resolve_domain();
        let cookies = match domain {
            Some(d) => {
                if let Some(c) = get_cookies_for_domain(
                    detector,
                    browsers,
                    args.browser.as_ref(),
                    args.profile.as_ref(),
                    d,
                )
                .await
                {
                    c
                } else {
                    if !args.quiet {
                        eprintln!("No cookies found for {} (domain: {d})", config.command);
                    }
                    continue;
                }
            }
            None => vec![],
        };
        agents.push(Agent::new(config.clone(), cookies));
    }
    agents
}

async fn print_json_status(agents: &[Agent]) {
    let mut statuses: Vec<AgentStatus> = Vec::new();
    for agent in agents {
        match agent.fetch_status().await {
            Ok(status) => statuses.push(status),
            Err(e) => eprintln!("Failed to fetch status for {}: {e}", agent.command()),
        }
    }
    match serde_json::to_string_pretty(&statuses) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Failed to serialize status: {e}"),
    }
}

async fn run_with_limit_check(settings: &Settings, agents: Vec<Agent>, args: &Args) {
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
            Ok(AgentLimit::NotLimited) => available_indices.push(*i),
            Ok(AgentLimit::Limited { reset_time }) => limited_indices.push((*i, *reset_time)),
            Err(e) => {
                if !args.quiet {
                    eprintln!("Failed to check limit for agent {i}: {e}");
                }
            }
        }
    }

    if let Some(model_key) = args.model.as_deref() {
        if !agents.iter().any(|a| a.has_model(model_key)) {
            eprintln!("No agents found with model '{model_key}'");
            return;
        }
        available_indices.retain(|&i| agents[i].has_model(model_key));
        limited_indices.retain(|(i, _)| agents[*i].has_model(model_key));
    }

    let stdin_prompt = {
        use std::io::{IsTerminal, Read};
        if std::io::stdin().is_terminal() {
            PromptState::Unresolved
        } else {
            let mut content = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut content)
                && !args.quiet
            {
                eprintln!("Failed to read stdin: {e}");
            }
            match parse_stdin_content(&content) {
                Some(s) => PromptState::Resolved(s),
                None => PromptState::Empty,
            }
        }
    };

    let mut input = InvocationInput {
        raw_agent_args: args.extra.clone(),
        cached_prompt: None,
        stdin_prompt,
    };

    if let Some(selected_index) =
        select_best_available_agent(settings, &agents, &available_indices, args.model.as_deref())
    {
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
        );
        return;
    }

    if !limited_indices.is_empty() {
        let earliest = limited_indices
            .iter()
            .filter_map(|(i, rt)| rt.map(|t| (*i, t)))
            .min_by_key(|(_, t)| *t);

        if let Some((idx, rt)) = earliest {
            if !args.quiet {
                println!(
                    "All agents limited. Waiting for {} ({} seconds)...",
                    rt.format("%Y-%m-%d %H:%M:%S UTC"),
                    (rt - Utc::now()).num_seconds()
                );
            }
            sleep_until_reset(rt, args.quiet).await;
            execute_with_auto_rerun(&agents, idx, &mut input, args.model.as_deref(), args.quiet);
            return;
        } else if !args.quiet {
            println!("All agents limited, no reset time available");
        }
    }

    eprintln!("No available agents");
}

fn collect_candidate_profiles_with<GetProfile, ListProfiles>(
    browsers: &[BrowserType],
    browser_arg: Option<&str>,
    profile_arg: Option<&str>,
    mut get_profile: GetProfile,
    mut list_profiles: ListProfiles,
) -> Vec<seher::Profile>
where
    GetProfile: FnMut(BrowserType, &str) -> Option<seher::Profile>,
    ListProfiles: FnMut(BrowserType) -> Vec<seher::Profile>,
{
    if let Some(browser_name) = browser_arg {
        let Ok(browser_type) = BrowserType::from_str(browser_name) else {
            return Vec::new();
        };

        if !browsers.contains(&browser_type) {
            return Vec::new();
        }

        return match profile_arg {
            Some(profile_name) => get_profile(browser_type, profile_name)
                .into_iter()
                .collect(),
            None => list_profiles(browser_type),
        };
    }

    let mut profiles = Vec::new();
    for browser in browsers {
        if !browser.is_chromium_based() {
            continue;
        }

        match profile_arg {
            Some(profile_name) => {
                if let Some(profile) = get_profile(*browser, profile_name) {
                    profiles.push(profile);
                }
            }
            None => profiles.extend(list_profiles(*browser)),
        }
    }

    profiles
}

fn collect_candidate_profiles(
    detector: &BrowserDetector,
    browsers: &[BrowserType],
    browser_arg: Option<&String>,
    profile_arg: Option<&String>,
) -> Vec<seher::Profile> {
    collect_candidate_profiles_with(
        browsers,
        browser_arg.map(String::as_str),
        profile_arg.map(String::as_str),
        |browser_type, profile_name| detector.get_profile(browser_type, Some(profile_name)),
        |browser_type| detector.list_profiles(browser_type),
    )
}

fn collect_cookie_candidates(
    detector: &BrowserDetector,
    browsers: &[BrowserType],
    browser_arg: Option<&String>,
    profile_arg: Option<&String>,
    domain: &str,
) -> Vec<Vec<seher::Cookie>> {
    collect_candidate_profiles(detector, browsers, browser_arg, profile_arg)
        .into_iter()
        .filter_map(|profile| CookieReader::read_cookies(&profile, domain).ok())
        .collect()
}

fn has_valid_session_cookie(domain: &str, cookie: &seher::Cookie) -> bool {
    has_session_cookie(domain, cookie) && !cookie.is_expired()
}

async fn select_cookie_candidate<F, Fut>(
    domain: &str,
    candidates: Vec<Vec<seher::Cookie>>,
    mut codex_validator: F,
) -> Option<Vec<seher::Cookie>>
where
    F: FnMut(Vec<seher::Cookie>) -> Fut,
    Fut: Future<Output = (Vec<seher::Cookie>, bool)>,
{
    for cookies in candidates {
        if !cookies
            .iter()
            .any(|cookie| has_valid_session_cookie(domain, cookie))
        {
            continue;
        }

        if domain == "chatgpt.com" {
            let (cookies, is_valid) = codex_validator(cookies).await;
            if !is_valid {
                continue;
            }
            return Some(cookies);
        }

        return Some(cookies);
    }

    None
}

async fn get_cookies_for_domain(
    detector: &BrowserDetector,
    browsers: &[BrowserType],
    browser_arg: Option<&String>,
    profile_arg: Option<&String>,
    domain: &str,
) -> Option<Vec<seher::Cookie>> {
    let candidates =
        collect_cookie_candidates(detector, browsers, browser_arg, profile_arg, domain);

    select_cookie_candidate(domain, candidates, |cookies| async move {
        let is_valid = CodexClient::session_has_access_token(&cookies)
            .await
            .unwrap_or(true);
        (cookies, is_valid)
    })
    .await
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

fn parse_stdin_content(content: &str) -> Option<String> {
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn prompt_from_editor() -> std::result::Result<String, Box<dyn std::error::Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    std::process::Command::new(&editor)
        .arg(tmp.path())
        .status()?;
    Ok(std::fs::read_to_string(tmp.path())?.trim().to_string())
}

fn execute_with_auto_rerun(
    agents: &[Agent],
    idx: usize,
    input: &mut InvocationInput,
    model: Option<&str>,
    quiet: bool,
) {
    let exit_kind = execute_agent(agents, idx, input, model, quiet);
    let provider_aware = agents[idx].config.resolve_provider().is_some();
    if should_auto_rerun(&exit_kind, provider_aware) {
        if !quiet {
            eprintln!("Agent failed, retrying...");
        }
        execute_agent(agents, idx, input, model, quiet);
    }
}

fn execute_agent(
    agents: &[Agent],
    selected_index: usize,
    input: &mut InvocationInput,
    model: Option<&str>,
    quiet: bool,
) -> ChildExitKind {
    let selected_agent = &agents[selected_index];
    let mut final_args = selected_agent.mapped_args(&input.raw_agent_args);

    if let PromptState::Resolved(p) = &input.stdin_prompt {
        final_args.push(p.clone());
    }

    if matches!(input.stdin_prompt, PromptState::Unresolved)
        && input.raw_agent_args.is_empty()
        && !quiet
    {
        if input.cached_prompt.is_none() {
            match prompt_from_editor() {
                Ok(prompt) => input.cached_prompt = Some(prompt),
                Err(e) => {
                    eprintln!("Editor error: {e}");
                    // SpawnError prevents auto-rerun, which is correct -- the agent was never started.
                    return ChildExitKind::SpawnError;
                }
            }
        }
        if let Some(p) = input.cached_prompt.as_deref()
            && !p.is_empty()
        {
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

fn format_priority_entry<W: std::io::Write>(
    writer: &mut W,
    rank: usize,
    config: &AgentConfig,
    priority: i32,
) {
    let provider = config.resolve_provider().unwrap_or("(none)");
    let env_display = match &config.env {
        None => String::new(),
        Some(env) => {
            let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.join(", ")
        }
    };
    writeln!(
        writer,
        "  {}. [priority={:3}] command={} provider={} env={{{}}}",
        rank, priority, config.command, provider, env_display
    )
    .ok();
}

fn write_model_section<W: std::io::Write>(
    writer: &mut W,
    settings: &Settings,
    model: Option<&str>,
) {
    let mut candidates: Vec<_> = settings
        .agents
        .iter()
        .enumerate()
        .filter(|(_, config)| {
            model.is_none_or(|key| config.models.as_ref().is_none_or(|m| m.contains_key(key)))
        })
        .map(|(i, config)| (i, config, settings.priority_for(config, model)))
        .collect();
    candidates.sort_by_key(|(i, _, priority)| (Reverse(*priority), *i));

    writeln!(writer, "Model: {}", model.unwrap_or("(none)")).ok();
    for (rank, (_, config, priority)) in candidates.iter().enumerate() {
        format_priority_entry(writer, rank + 1, config, *priority);
    }
    writeln!(writer).ok();
}

fn write_priority<W: std::io::Write>(writer: &mut W, settings: &Settings) {
    use std::collections::BTreeSet;

    let model_keys: BTreeSet<String> = settings
        .agents
        .iter()
        .filter_map(|config| config.models.as_ref())
        .flat_map(|m| m.keys().cloned())
        .collect();

    for key in &model_keys {
        write_model_section(writer, settings, Some(key.as_str()));
    }
    write_model_section(writer, settings, None);
}

fn print_priority(settings: &Settings) {
    write_priority(&mut std::io::stdout(), settings);
}

async fn sleep_until_reset(reset_time: DateTime<Utc>, quiet: bool) {
    let now = Utc::now();
    if reset_time <= now {
        if !quiet {
            println!("\nReset time has already passed, no sleep needed.");
        }
        return;
    }

    let total_secs = (reset_time - now).num_seconds().max(0).cast_unsigned();
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
    use seher::{AgentConfig, PriorityRule, config::ProviderConfig};
    use std::collections::HashMap;

    fn sample_cookie(name: &str) -> seher::Cookie {
        sample_cookie_with_value(name, "value", i64::MAX)
    }

    fn sample_cookie_with_value(name: &str, value: &str, expires_utc: i64) -> seher::Cookie {
        seher::Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: ".chatgpt.com".to_string(),
            path: "/".to_string(),
            expires_utc,
            is_secure: true,
            is_httponly: true,
            same_site: 0,
        }
    }

    fn sample_profile(name: &str, browser_type: BrowserType) -> seher::Profile {
        seher::Profile::new(
            name.to_string(),
            PathBuf::from(format!("/tmp/{}/{}", browser_type.name(), name)),
            browser_type,
        )
    }

    fn sample_agent(command: &str, provider: Option<ProviderConfig>) -> Agent {
        Agent::new(
            AgentConfig {
                command: command.to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
                provider,
                openrouter_management_key: None,
                pre_command: vec![],
            },
            vec![],
        )
    }

    fn sample_settings_with_priority(priority: Vec<PriorityRule>) -> Settings {
        let mut s = Settings::default();
        s.priority = priority;
        s.agents = vec![];
        s
    }

    // -----------------------------------------------------------------------
    // filter_agents
    // -----------------------------------------------------------------------

    #[test]
    fn filter_agents_by_command() {
        let agents = vec![sample_agent("claude", None), sample_agent("codex", None)];
        let result = filter_agents(agents, Some("claude"), None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command(), "claude");
    }

    #[test]
    fn filter_agents_by_provider() {
        let agents = vec![
            sample_agent(
                "opencode",
                Some(ProviderConfig::Explicit("copilot".to_string())),
            ),
            sample_agent("codex", None),
        ];
        let result = filter_agents(agents, None, Some("copilot"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command(), "opencode");
    }

    #[test]
    fn filter_agents_by_command_and_provider_and_condition() {
        let agents = vec![
            sample_agent(
                "opencode",
                Some(ProviderConfig::Explicit("copilot".to_string())),
            ),
            sample_agent("opencode", None),
            sample_agent(
                "codex",
                Some(ProviderConfig::Explicit("copilot".to_string())),
            ),
        ];
        let result = filter_agents(agents, Some("opencode"), Some("copilot"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command(), "opencode");
    }

    #[test]
    fn filter_agents_no_match_returns_empty() {
        let agents = vec![sample_agent("claude", None), sample_agent("codex", None)];
        let result = filter_agents(agents, Some("nonexistent"), None);
        assert!(result.is_empty());
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
    fn select_best_available_agent_prefers_higher_priority() {
        let settings = sample_settings_with_priority(vec![
            PriorityRule {
                command: "claude".to_string(),
                provider: None,
                model: Some("high".to_string()),
                priority: 10,
            },
            PriorityRule {
                command: "codex".to_string(),
                provider: None,
                model: Some("high".to_string()),
                priority: 100,
            },
        ]);
        let agents = vec![sample_agent("claude", None), sample_agent("codex", None)];

        let selected = select_best_available_agent(&settings, &agents, &[0, 1], Some("high"));

        assert_eq!(selected, Some(1));
    }

    #[test]
    fn select_best_available_agent_keeps_agent_order_when_priority_is_equal() {
        let settings = sample_settings_with_priority(vec![]);
        let agents = vec![sample_agent("claude", None), sample_agent("codex", None)];

        let selected = select_best_available_agent(&settings, &agents, &[1, 0], Some("high"));

        assert_eq!(selected, Some(0));
    }

    #[test]
    fn select_best_available_agent_uses_model_specific_priority() {
        let settings = sample_settings_with_priority(vec![
            PriorityRule {
                command: "claude".to_string(),
                provider: None,
                model: Some("high".to_string()),
                priority: 100,
            },
            PriorityRule {
                command: "claude".to_string(),
                provider: None,
                model: Some("low".to_string()),
                priority: -50,
            },
        ]);
        let agents = vec![sample_agent("claude", None)];

        let selected_high = select_best_available_agent(&settings, &agents, &[0], Some("high"));
        let selected_low = select_best_available_agent(&settings, &agents, &[0], Some("low"));

        assert_eq!(selected_high, Some(0));
        assert_eq!(selected_low, Some(0));
        assert_eq!(settings.priority_for(&agents[0].config, Some("high")), 100);
        assert_eq!(settings.priority_for(&agents[0].config, Some("low")), -50);
    }

    #[test]
    fn select_best_available_agent_allows_fallback_to_win_on_priority() {
        let settings = sample_settings_with_priority(vec![
            PriorityRule {
                command: "claude".to_string(),
                provider: None,
                model: Some("medium".to_string()),
                priority: 10,
            },
            PriorityRule {
                command: "claude".to_string(),
                provider: Some(ProviderConfig::None),
                model: Some("medium".to_string()),
                priority: 20,
            },
        ]);
        let agents = vec![
            sample_agent("claude", None),
            sample_agent("claude", Some(ProviderConfig::None)),
        ];

        let selected = select_best_available_agent(&settings, &agents, &[0, 1], Some("medium"));

        assert_eq!(selected, Some(1));
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

    #[test]
    fn collect_candidate_profiles_filters_named_profiles_without_browser_arg() {
        let chrome_default = sample_profile("Default", BrowserType::Chrome);
        let chrome_profile = sample_profile("Profile 17", BrowserType::Chrome);
        let edge_profile = sample_profile("Profile 17", BrowserType::Edge);
        let firefox_profile = sample_profile("Profile 17", BrowserType::Firefox);

        let profiles = collect_candidate_profiles_with(
            &[BrowserType::Chrome, BrowserType::Edge, BrowserType::Firefox],
            None,
            Some("Profile 17"),
            |browser_type, profile_name| match (browser_type, profile_name) {
                (BrowserType::Chrome, "Profile 17") => Some(chrome_profile.clone()),
                (BrowserType::Edge, "Profile 17") => Some(edge_profile.clone()),
                (BrowserType::Firefox, "Profile 17") => Some(firefox_profile.clone()),
                _ => None,
            },
            |browser_type| match browser_type {
                BrowserType::Chrome => vec![chrome_default.clone(), chrome_profile.clone()],
                BrowserType::Edge => vec![edge_profile.clone()],
                BrowserType::Firefox => vec![firefox_profile.clone()],
                _ => vec![],
            },
        );

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].browser_type, BrowserType::Chrome);
        assert_eq!(profiles[0].name, "Profile 17");
        assert_eq!(profiles[1].browser_type, BrowserType::Edge);
        assert_eq!(profiles[1].name, "Profile 17");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn select_cookie_candidate_skips_codex_profiles_without_access_token() -> TestResult {
        let invalid = vec![sample_cookie_with_value(
            "__Secure-next-auth.session-token.0",
            "invalid",
            i64::MAX,
        )];
        let valid = vec![sample_cookie_with_value(
            "__Secure-next-auth.session-token.0",
            "valid",
            i64::MAX,
        )];

        let selected = select_cookie_candidate(
            "chatgpt.com",
            vec![invalid, valid.clone()],
            |cookies| async move {
                let is_valid = cookies.iter().any(|cookie| cookie.value == "valid");
                (cookies, is_valid)
            },
        )
        .await;

        let selected = selected.ok_or("expected Some")?;
        assert_eq!(selected[0].value, valid[0].value);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn select_cookie_candidate_ignores_expired_session_cookies() -> TestResult {
        let expired = vec![sample_cookie_with_value(
            "__Secure-next-auth.session-token.0",
            "expired",
            11_644_473_601_000_000,
        )];
        let valid = vec![sample_cookie_with_value(
            "__Secure-next-auth.session-token.0",
            "valid",
            i64::MAX,
        )];

        let selected = select_cookie_candidate(
            "chatgpt.com",
            vec![expired, valid.clone()],
            |cookies| async move { (cookies, true) },
        )
        .await;

        let selected = selected.ok_or("expected Some")?;
        assert_eq!(selected[0].value, valid[0].value);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // write_priority
    // -----------------------------------------------------------------------

    fn make_priority_settings() -> Settings {
        let mut claude_models = HashMap::new();
        claude_models.insert("high".to_string(), "opus".to_string());
        claude_models.insert("low".to_string(), "haiku".to_string());

        let mut s = Settings::default();
        s.priority = vec![
            PriorityRule {
                command: "claude".to_string(),
                provider: None,
                model: Some("high".to_string()),
                priority: 10,
            },
            PriorityRule {
                command: "codex".to_string(),
                provider: None,
                model: None,
                priority: 50,
            },
        ];
        s.agents = vec![
            AgentConfig {
                command: "claude".to_string(),
                args: vec![],
                models: Some(claude_models),
                arg_maps: HashMap::new(),
                env: Some({
                    let mut e = HashMap::new();
                    e.insert("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string());
                    e
                }),
                provider: None,
                openrouter_management_key: None,
                pre_command: vec![],
            },
            AgentConfig {
                command: "codex".to_string(),
                args: vec![],
                models: None,
                arg_maps: HashMap::new(),
                env: None,
                provider: None,
                openrouter_management_key: None,
                pre_command: vec![],
            },
        ];
        s
    }

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn write_priority_contains_model_sections() -> TestResult {
        let settings = make_priority_settings();
        let mut output = Vec::new();
        write_priority(&mut output, &settings);
        let output = String::from_utf8(output)?;

        assert!(
            output.contains("Model: high"),
            "should have Model: high section"
        );
        assert!(
            output.contains("Model: low"),
            "should have Model: low section"
        );
        assert!(
            output.contains("Model: (none)"),
            "should have Model: (none) section"
        );
        Ok(())
    }

    #[test]
    fn write_priority_sorts_by_priority_descending_in_none_section() -> TestResult {
        let settings = make_priority_settings();
        let mut output = Vec::new();
        write_priority(&mut output, &settings);
        let output = String::from_utf8(output)?;

        // In Model: (none): codex (priority=50) should come before claude (priority=0)
        let none_start = output
            .find("Model: (none)")
            .ok_or("Model: (none) not found")?;
        let none_section = &output[none_start..];
        let codex_pos = none_section
            .find("command=codex")
            .ok_or("command=codex not found")?;
        let claude_pos = none_section
            .find("command=claude")
            .ok_or("command=claude not found")?;
        assert!(
            codex_pos < claude_pos,
            "codex (priority=50) should precede claude (priority=0)"
        );
        Ok(())
    }

    #[test]
    fn write_priority_includes_passthrough_agent_in_model_sections() -> TestResult {
        let settings = make_priority_settings();
        let mut output = Vec::new();
        write_priority(&mut output, &settings);
        let output = String::from_utf8(output)?;

        // In Model: high: claude (priority=10) should come before codex (priority=0, pass-through)
        let high_start = output.find("Model: high").ok_or("Model: high not found")?;
        let high_end = output[high_start..]
            .find("\nModel:")
            .map_or(output.len(), |p| high_start + p);
        let high_section = &output[high_start..high_end];

        assert!(
            high_section.contains("command=claude"),
            "claude should be in Model: high"
        );
        assert!(
            high_section.contains("command=codex"),
            "codex (pass-through) should be in Model: high"
        );

        let claude_pos = high_section
            .find("command=claude")
            .ok_or("command=claude not found")?;
        let codex_pos = high_section
            .find("command=codex")
            .ok_or("command=codex not found")?;
        assert!(
            claude_pos < codex_pos,
            "claude (priority=10) should precede codex (priority=0) in high section"
        );
        Ok(())
    }

    #[test]
    fn write_priority_formats_env_keys_sorted() -> TestResult {
        let settings = make_priority_settings();
        let mut output = Vec::new();
        write_priority(&mut output, &settings);
        let output = String::from_utf8(output)?;

        assert!(
            output.contains("env={ANTHROPIC_API_KEY}"),
            "should show env key for claude"
        );
        assert!(output.contains("env={}"), "should show empty env for codex");
        Ok(())
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
    fn child_exit_kind_from_returns_success_for_zero_exit() -> TestResult {
        // Given: a process that exits cleanly with code 0
        // When:  converting to ChildExitKind
        // Then:  returns Success
        let status = std::process::Command::new("true").status()?;
        assert_eq!(ChildExitKind::from(Ok(status)), ChildExitKind::Success);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn child_exit_kind_from_returns_failure_for_nonzero_exit() -> TestResult {
        // Given: a process that exits with code 1
        // When:  converting to ChildExitKind
        // Then:  returns Failure with code Some(1)
        let status = std::process::Command::new("false").status()?;
        assert_eq!(
            ChildExitKind::from(Ok(status)),
            ChildExitKind::Failure { code: Some(1) },
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn child_exit_kind_from_returns_signal_terminated_for_signal_killed_process() -> TestResult {
        // Given: a process terminated by SIGKILL (Unix signal)
        // When:  converting to ChildExitKind
        // Then:  returns SignalTerminated (signal termination != voluntary non-zero exit)
        use std::os::unix::process::ExitStatusExt;
        // Spawn a long-running process and kill it with SIGKILL
        let mut child = std::process::Command::new("sleep").arg("60").spawn()?;
        child.kill()?;
        let status = child.wait()?;
        // SIGKILL (signal 9) -> signal() returns Some(9), code() returns None
        assert!(status.signal().is_some());
        assert_eq!(
            ChildExitKind::from(Ok(status)),
            ChildExitKind::SignalTerminated
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // parse_stdin_content
    // -----------------------------------------------------------------------

    #[test]
    fn parse_stdin_content_returns_some_for_nonempty_string() {
        assert_eq!(
            parse_stdin_content("fix bugs"),
            Some("fix bugs".to_string())
        );
    }

    #[test]
    fn parse_stdin_content_trims_leading_and_trailing_whitespace() {
        assert_eq!(
            parse_stdin_content("  fix bugs  \n"),
            Some("fix bugs".to_string())
        );
    }

    #[test]
    fn parse_stdin_content_returns_none_for_empty_string() {
        assert_eq!(parse_stdin_content(""), None);
    }

    #[test]
    fn parse_stdin_content_returns_none_for_whitespace_only_string() {
        assert_eq!(parse_stdin_content("  \n  \t  "), None);
    }

    #[test]
    fn parse_stdin_content_preserves_internal_content_including_newlines() {
        assert_eq!(
            parse_stdin_content("line one\nline two\n"),
            Some("line one\nline two".to_string())
        );
    }
}
