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
    about = "CLI tool for Claude.ai and Copilot rate limit monitoring"
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
        let domain = match get_domain_for_command(&config.command) {
            Some(d) => d,
            None => {
                eprintln!(
                    "Warning: unrecognized command '{}', skipping",
                    config.command
                );
                continue;
            }
        };

        let cookies = match get_cookies_for_domain(
            &detector,
            &browsers,
            &args.browser,
            &args.profile,
            domain,
        ) {
            Some(c) => c,
            None => {
                if !args.quiet {
                    eprintln!(
                        "No cookies found for {} (domain: {})",
                        config.command, domain
                    );
                }
                continue;
            }
        };

        agents.push(Agent::new(config.clone(), cookies, domain.to_string()));
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

    if !available_indices.is_empty() {
        let selected_index = available_indices[0];
        if !args.quiet {
            println!(
                "Agent {} is available (not limited)",
                agents[selected_index].command()
            );
        }
        execute_agent(
            &agents,
            selected_index,
            args.agent_args,
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
            execute_agent(
                &agents,
                idx,
                args.agent_args,
                args.model.as_deref(),
                args.quiet,
            )
            .await;
            return;
        } else if !args.quiet {
            println!("All agents limited, no reset time available");
        }
    }

    eprintln!("No available agents");
}

fn get_domain_for_command(command: &str) -> Option<&str> {
    match command {
        "claude" => Some("claude.ai"),
        "copilot" => Some("github.com"),
        _ => None,
    }
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

async fn execute_agent(
    agents: &[Agent],
    selected_index: usize,
    agent_args: Vec<String>,
    model: Option<&str>,
    quiet: bool,
) {
    let selected_agent = &agents[selected_index];
    let mut final_args = agent_args;

    if final_args.is_empty() && !quiet {
        match prompt_from_editor().await {
            Ok(prompt) if !prompt.is_empty() => final_args.push(prompt),
            Ok(_) => {}
            Err(e) => {
                eprintln!("Editor error: {}", e);
                return;
            }
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

    if let Err(e) = selected_agent.execute(&resolved, &final_args) {
        eprintln!("Failed to execute agent: {}", e);
    }
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
