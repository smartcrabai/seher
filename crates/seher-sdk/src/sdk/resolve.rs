//! Provider resolution engine.
//!
//! Mirrors `seher-ts/packages/sdk/src/sdk/resolve.ts`:
//!  * Candidate list = providers that define `models[mode_key]`, filtered by
//!    `provider`/`exclude`, sorted by `priority` desc then YAML `order` asc.
//!  * Probe each in order via [`CodexBarProbe`] (the external `codexbar` binary);
//!    first non-limited wins. If all limited and `!no_wait`/within `max_rescans`,
//!    sleep until earliest reset and rescan.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};

use crate::codexbar::AgentLimit;

use super::config::{Config, ProviderEntry, ResolvedAgent};
use super::config_loader::{ConfigError, load_config};
use super::sleep::sleep_until;

/// Boxed probe future returned by [`LimitProbe::probe`].
pub type ProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AgentLimit, Box<dyn std::error::Error>>> + 'a>>;

/// Trait for "ask whether this provider candidate is at-limit." Production uses
/// [`CodexBarProbe`] (the external `codexbar` binary); tests inject mocks.
pub trait LimitProbe {
    fn probe<'a>(
        &'a mut self,
        entry: &'a ProviderEntry,
        resolved: &'a ResolvedAgent,
    ) -> ProbeFuture<'a>;
}

#[derive(Debug, thiserror::Error)]
#[error("All providers are rate-limited; earliest reset at {0:?}")]
pub struct AllAgentsLimitedError(pub Option<DateTime<Utc>>);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NoMatchingAgentError(pub String);

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    AllLimited(#[from] AllAgentsLimitedError),
    #[error(transparent)]
    NoMatching(#[from] NoMatchingAgentError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("Resolution canceled")]
    Canceled,
}

#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub mode_key: String,
    pub provider_filter: Option<String>,
    pub config_path: Option<PathBuf>,
    pub config: Option<Config>,
    pub exclude_providers: Vec<String>,
    pub no_wait: bool,
    pub max_rescans: u32,
    pub quiet: bool,
    /// When true, the run will pass custom tools, so only SDKs that support
    /// function calling (`pi`) are eligible. `claude-terminal` candidates are
    /// dropped (see [`sdk_supports_tools`]).
    pub require_tools: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            mode_key: "build".to_string(),
            provider_filter: None,
            config_path: None,
            config: None,
            exclude_providers: Vec::new(),
            no_wait: false,
            max_rescans: 1,
            quiet: false,
            require_tools: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PollOptions {
    pub mode_key: String,
    pub provider_filter: Option<String>,
    pub config_path: Option<PathBuf>,
    pub config: Option<Config>,
    pub exclude_providers: Vec<String>,
    pub interval_ms: u64,
    pub cancel: Option<Arc<AtomicBool>>,
    /// When true, the run will pass custom tools, so only SDKs that support
    /// function calling (`pi`) are eligible. `claude-terminal` candidates are
    /// dropped (see [`sdk_supports_tools`]).
    pub require_tools: bool,
}

impl Default for PollOptions {
    fn default() -> Self {
        Self {
            mode_key: "build".to_string(),
            provider_filter: None,
            config_path: None,
            config: None,
            exclude_providers: Vec::new(),
            interval_ms: 60_000,
            cancel: None,
            require_tools: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub priority: i32,
    pub order: usize,
    pub entry_index: usize,
    pub resolved: ResolvedAgent,
}

/// Supported `sdk` values that can actually be executed by this implementation.
///
/// `pi_agent_rust` is the in-process execution engine; `claude-terminal` drives
/// the local `claude` CLI via tmux; `claude-headless` drives `claude -p` as a
/// subprocess. Providers tagged with other seher-ts-only SDK kinds (`claude`,
/// `codex`, `copilot`, `cursor`, `kimi`, `opencode`) cannot be run here. The
/// config still accepts them (so the same `config.yaml` works in both
/// implementations); they are silently filtered out of the candidate list.
pub const SUPPORTED_SDK_KINDS: &[&str] = &["pi", "claude-terminal", "claude-headless"];

#[must_use]
pub fn is_supported_sdk(sdk: &str) -> bool {
    SUPPORTED_SDK_KINDS.contains(&sdk)
}

/// Whether `sdk` can execute custom tools (function calling). Only the in-process
/// `pi` engine can; `claude-terminal` drives the `claude` CLI via tmux and cannot
/// inject tool definitions into it.
#[must_use]
pub fn sdk_supports_tools(sdk: &str) -> bool {
    sdk == "pi"
}

/// Apply the `require_tools` filter to `candidates` and produce the final list,
/// or a [`NoMatchingAgentError`] explaining why it came up empty. Shared by
/// [`resolve_agent`] and [`poll_for_agent`].
fn filter_candidates_for_tools(
    mut candidates: Vec<Candidate>,
    mode_key: &str,
    provider_filter: Option<&str>,
    require_tools: bool,
) -> Result<Vec<Candidate>, NoMatchingAgentError> {
    // Custom tools only run on the in-process pi engine; drop claude-terminal here
    // so resolution never lands on a backend that can't honor the requested tools.
    let dropped_for_tools = if require_tools {
        let before = candidates.len();
        candidates.retain(|c| sdk_supports_tools(&c.resolved.sdk));
        before - candidates.len()
    } else {
        0
    };
    if candidates.is_empty() {
        let msg = if dropped_for_tools > 0 {
            format!(
                "No tool-capable (pi) provider defines models.{mode_key}; {dropped_for_tools} candidate(s) were excluded because custom tools are not supported on claude-terminal/claude-headless"
            )
        } else if let Some(p) = provider_filter {
            format!("No provider \"{p}\" defines models.{mode_key}")
        } else {
            format!("No providers define models.{mode_key}")
        };
        return Err(NoMatchingAgentError(msg));
    }
    Ok(candidates)
}

/// Enumerate `(provider, sdk)` pairs from the YAML config whose `sdk` value is
/// not executable by this implementation. Use to print a one-time warning at
/// startup. Returns an empty Vec when every provider is runnable.
#[must_use]
pub fn unsupported_sdk_providers(cfg: &Config) -> Vec<(String, String)> {
    cfg.providers
        .iter()
        .filter(|p| !is_supported_sdk(&p.sdk))
        .map(|p| (p.provider.clone(), p.sdk.clone()))
        .collect()
}

#[must_use]
pub fn build_candidates(
    cfg: &Config,
    mode_key: &str,
    provider_filter: Option<&str>,
    exclude: &[String],
) -> Vec<Candidate> {
    let exclude_set: HashSet<&str> = exclude.iter().map(String::as_str).collect();
    let mut out: Vec<Candidate> = cfg
        .providers
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            // Drop providers whose `sdk` isn't executable here (e.g. seher-ts
            // entries with `sdk: claude`). The same config.yaml is portable.
            if !is_supported_sdk(&entry.sdk) {
                return None;
            }
            if let Some(p) = provider_filter
                && entry.provider != p
            {
                return None;
            }
            if exclude_set.contains(entry.provider.as_str()) {
                return None;
            }
            let model = entry.models.get(mode_key)?;
            let priority = model.priority.or(entry.priority).unwrap_or(0);
            let skills = cfg.resolve_skills(entry);
            let resolved = ResolvedAgent {
                provider: entry.provider.clone(),
                model_id: model.model.clone(),
                mode_key: mode_key.to_string(),
                sdk: entry.sdk.clone(),
                api: entry.api.clone(),
                skills,
            };
            Some(Candidate {
                priority,
                order: entry.order,
                entry_index: i,
                resolved,
            })
        })
        .collect();
    out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.order.cmp(&b.order)));
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanOutcome {
    Available {
        idx: usize,
    },
    AllLimited {
        reset_time: Option<DateTime<Utc>>,
    },
    /// No candidates were available — either the list was empty, or every probe
    /// errored. `probe_errors` records the latter so callers can surface root cause.
    NoAgents {
        probe_errors: Vec<(String, String)>,
    },
}

/// Probe each candidate in order; return the first non-limited, otherwise summarise.
pub async fn scan(
    candidates: &[Candidate],
    entries: &[ProviderEntry],
    probe: &mut dyn LimitProbe,
) -> ScanOutcome {
    if candidates.is_empty() {
        return ScanOutcome::NoAgents {
            probe_errors: Vec::new(),
        };
    }
    let mut limited: Vec<Option<DateTime<Utc>>> = Vec::new();
    let mut probe_errors: Vec<(String, String)> = Vec::new();
    for (idx, c) in candidates.iter().enumerate() {
        let entry = &entries[c.entry_index];
        match probe.probe(entry, &c.resolved).await {
            Ok(AgentLimit::NotLimited) => return ScanOutcome::Available { idx },
            Ok(AgentLimit::Limited { reset_time }) => limited.push(reset_time),
            Err(e) => probe_errors.push((entry.provider.clone(), e.to_string())),
        }
    }
    if limited.is_empty() {
        ScanOutcome::NoAgents { probe_errors }
    } else {
        ScanOutcome::AllLimited {
            reset_time: limited.into_iter().flatten().min(),
        }
    }
}

fn format_probe_errors(errors: &[(String, String)]) -> String {
    let mut s = String::from("No available providers");
    if !errors.is_empty() {
        s.push_str(" (probe failures: ");
        for (i, (provider, msg)) in errors.iter().enumerate() {
            if i > 0 {
                s.push_str("; ");
            }
            let _ = write!(s, "{provider}: {msg}");
        }
        s.push(')');
    }
    s
}

/// Resolve the highest-priority non-limited provider for `mode_key`.
///
/// # Errors
///
/// Returns [`ResolveError::NoMatching`] when no provider defines the mode key
/// (or all error out during probing), [`ResolveError::AllLimited`] when every
/// candidate is at-limit and `no_wait`/`max_rescans` are exhausted, or
/// [`ResolveError::Config`] on config-loading failures.
pub async fn resolve_agent(
    opts: ResolveOptions,
    probe: &mut dyn LimitProbe,
) -> Result<ResolvedAgent, ResolveError> {
    let config = match opts.config.clone() {
        Some(c) => c,
        None => load_config(opts.config_path.as_deref())?,
    };
    let candidates = filter_candidates_for_tools(
        build_candidates(
            &config,
            &opts.mode_key,
            opts.provider_filter.as_deref(),
            &opts.exclude_providers,
        ),
        &opts.mode_key,
        opts.provider_filter.as_deref(),
        opts.require_tools,
    )?;

    let mut rescans: u32 = 0;
    loop {
        match scan(&candidates, &config.providers, probe).await {
            ScanOutcome::Available { idx } => return Ok(candidates[idx].resolved.clone()),
            ScanOutcome::NoAgents { probe_errors } => {
                return Err(NoMatchingAgentError(format_probe_errors(&probe_errors)).into());
            }
            ScanOutcome::AllLimited { reset_time } => {
                if opts.no_wait || rescans >= opts.max_rescans {
                    return Err(AllAgentsLimitedError(reset_time).into());
                }
                if let Some(when) = reset_time {
                    sleep_until(when, opts.quiet).await;
                }
                rescans = rescans.saturating_add(1);
            }
        }
    }
}

/// Like [`resolve_agent`] but loops forever (until cancelled), sleeping
/// `interval_ms` between scans when every candidate is at-limit.
///
/// # Errors
///
/// Returns [`ResolveError::Canceled`] when the cancel signal flips,
/// [`ResolveError::NoMatching`] / [`ResolveError::Config`] on config issues.
pub async fn poll_for_agent(
    opts: PollOptions,
    probe: &mut dyn LimitProbe,
) -> Result<ResolvedAgent, ResolveError> {
    let config = match opts.config.clone() {
        Some(c) => c,
        None => load_config(opts.config_path.as_deref())?,
    };
    let candidates = filter_candidates_for_tools(
        build_candidates(
            &config,
            &opts.mode_key,
            opts.provider_filter.as_deref(),
            &opts.exclude_providers,
        ),
        &opts.mode_key,
        opts.provider_filter.as_deref(),
        opts.require_tools,
    )?;
    loop {
        if let Some(c) = &opts.cancel
            && c.load(Ordering::SeqCst)
        {
            return Err(ResolveError::Canceled);
        }
        match scan(&candidates, &config.providers, probe).await {
            ScanOutcome::Available { idx } => return Ok(candidates[idx].resolved.clone()),
            ScanOutcome::NoAgents { probe_errors } => {
                return Err(NoMatchingAgentError(format_probe_errors(&probe_errors)).into());
            }
            ScanOutcome::AllLimited { .. } => {
                let interval = opts.interval_ms.max(1);
                let until_ms = i64::try_from(interval).unwrap_or(i64::MAX);
                let until = Utc::now() + chrono::Duration::milliseconds(until_ms);
                sleep_until(until, true).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CodexBar-based probe (mirrors seher-ts, the production limit checker)
// ---------------------------------------------------------------------------

/// Map a YAML `(sdk, provider)` pair to the provider name codexbar expects.
///
/// Mirrors seher-ts `codexbarProviderName`: the `claude-terminal` and
/// `claude-headless` SDKs both drive the Claude CLI which authenticates as the
/// `claude` account, so they share that account's codexbar quota.
#[must_use]
pub fn codexbar_provider_name(sdk: &str, provider: &str) -> String {
    match sdk {
        "claude-terminal" | "claude-headless" => "claude".to_string(),
        _ => provider.to_string(),
    }
}

/// Limit probe backed by the external `codexbar` binary. This is the production
/// probe: it mirrors seher-ts, where limit determination is delegated entirely
/// to `CodexBar` rather than per-provider cookie/API checks.
pub struct CodexBarProbe;

impl LimitProbe for CodexBarProbe {
    fn probe<'a>(
        &'a mut self,
        entry: &'a ProviderEntry,
        resolved: &'a ResolvedAgent,
    ) -> ProbeFuture<'a> {
        Box::pin(async move {
            let provider = codexbar_provider_name(&resolved.sdk, &entry.provider);
            match crate::codexbar::check_limit(&provider).await {
                Ok(limit) => Ok(limit),
                // A missing codexbar entry (community providers), an absent
                // binary, or a transient spawn/timeout failure all mean "we
                // can't prove this provider is limited" — treat it as available
                // so resolution proceeds rather than dropping the provider.
                Err(_) => Ok(AgentLimit::NotLimited),
            }
        })
    }
}

/// Convenience wrapper: build a [`CodexBarProbe`] and run [`resolve_agent`].
///
/// # Errors
///
/// Same as [`resolve_agent`].
pub async fn resolve_agent_with_codexbar(
    opts: ResolveOptions,
) -> Result<ResolvedAgent, ResolveError> {
    let mut probe = CodexBarProbe;
    resolve_agent(opts, &mut probe).await
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::sdk::config::{ModelEntry, ProviderApi, ProviderEntry, SkillsConfig};
    use indexmap::IndexMap;

    fn entry(
        key: &str,
        provider: &str,
        priority: Option<i32>,
        models: &[(&str, &str, Option<i32>)],
    ) -> ProviderEntry {
        let mut m = IndexMap::new();
        for (k, model, pri) in models {
            m.insert(
                (*k).to_string(),
                ModelEntry {
                    model: (*model).to_string(),
                    priority: *pri,
                },
            );
        }
        ProviderEntry {
            key: key.to_string(),
            order: 0,
            provider: provider.to_string(),
            sdk: "pi".to_string(),
            priority,
            api: None,
            skills: None,
            models: m,
        }
    }

    fn cfg(providers: Vec<ProviderEntry>) -> Config {
        let providers = providers
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                e.order = i;
                e
            })
            .collect();
        Config {
            providers,
            skills: None,
        }
    }

    struct MockProbe {
        outcomes: HashMap<String, AgentLimit>,
    }

    impl LimitProbe for MockProbe {
        fn probe<'a>(
            &'a mut self,
            entry: &'a ProviderEntry,
            _resolved: &'a ResolvedAgent,
        ) -> ProbeFuture<'a> {
            let provider = entry.provider.clone();
            let outcome = self
                .outcomes
                .get(&provider)
                .cloned()
                .unwrap_or(AgentLimit::NotLimited);
            Box::pin(async move { Ok(outcome) })
        }
    }

    #[test]
    fn build_candidates_sorts_priority_desc_then_order_asc() {
        let c = cfg(vec![
            entry("a", "a", Some(1), &[("build", "x", None)]),
            entry("b", "b", Some(3), &[("build", "y", None)]),
            entry("c", "c", Some(3), &[("build", "z", None)]),
            entry("d", "d", None, &[("plan", "p", None)]),
        ]);
        let candidates = build_candidates(&c, "build", None, &[]);
        let providers: Vec<&str> = candidates
            .iter()
            .map(|c| c.resolved.provider.as_str())
            .collect();
        assert_eq!(providers, vec!["b", "c", "a"]);
    }

    #[test]
    fn build_candidates_uses_model_priority_over_provider() {
        let c = cfg(vec![
            entry("a", "a", Some(1), &[("build", "x", Some(10))]),
            entry("b", "b", Some(5), &[("build", "y", None)]),
        ]);
        let candidates = build_candidates(&c, "build", None, &[]);
        assert_eq!(candidates[0].resolved.provider, "a");
    }

    #[test]
    fn provider_filter_restricts_to_matching_provider() {
        let c = cfg(vec![
            entry("a", "a", Some(1), &[("build", "x", None)]),
            entry("b", "b", Some(2), &[("build", "y", None)]),
        ]);
        let candidates = build_candidates(&c, "build", Some("a"), &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].resolved.provider, "a");
    }

    #[test]
    fn exclude_filters_out_providers() {
        let c = cfg(vec![
            entry("a", "a", Some(1), &[("build", "x", None)]),
            entry("b", "b", Some(2), &[("build", "y", None)]),
        ]);
        let exclude = vec!["b".to_string()];
        let candidates = build_candidates(&c, "build", None, &exclude);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].resolved.provider, "a");
    }

    #[test]
    fn resolved_agent_carries_api_and_skills() {
        let mut e = entry("zai", "zai", None, &[("build", "zai/glm-5.1", None)]);
        e.api = Some(ProviderApi {
            key: Some("sk-test".into()),
            endpoint: Some("https://api.example.com".into()),
        });
        e.skills = Some(SkillsConfig {
            include_claude: Some(false),
        });
        let c = cfg(vec![e]);
        let candidates = build_candidates(&c, "build", None, &[]);
        let r = &candidates[0].resolved;
        assert_eq!(r.model_id, "zai/glm-5.1");
        assert_eq!(
            r.api.as_ref().and_then(|a| a.key.as_deref()),
            Some("sk-test")
        );
        assert!(!r.skills.include_claude);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_returns_highest_priority_not_limited() {
        let c = cfg(vec![
            entry("a", "a", Some(1), &[("build", "x", None)]),
            entry("b", "b", Some(3), &[("build", "y", None)]),
            entry("c", "c", Some(2), &[("build", "z", None)]),
        ]);
        let mut limits = HashMap::new();
        limits.insert(
            "b".to_string(),
            AgentLimit::Limited {
                reset_time: Some(Utc::now() + chrono::Duration::seconds(60)),
            },
        );
        let mut probe = MockProbe { outcomes: limits };
        let opts = ResolveOptions {
            config: Some(c),
            no_wait: true,
            ..Default::default()
        };
        let resolved = resolve_agent(opts, &mut probe).await.expect("resolve");
        assert_eq!(resolved.provider, "c");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_returns_no_matching_for_unknown_mode() {
        let c = cfg(vec![entry("a", "a", Some(1), &[("build", "x", None)])]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = ResolveOptions {
            config: Some(c),
            mode_key: "plan".to_string(),
            ..Default::default()
        };
        let err = resolve_agent(opts, &mut probe)
            .await
            .expect_err("should fail");
        assert!(matches!(err, ResolveError::NoMatching(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_throws_all_limited_when_no_wait() {
        let c = cfg(vec![entry("a", "a", Some(1), &[("build", "x", None)])]);
        let mut limits = HashMap::new();
        limits.insert(
            "a".to_string(),
            AgentLimit::Limited {
                reset_time: Some(Utc::now() + chrono::Duration::seconds(60)),
            },
        );
        let mut probe = MockProbe { outcomes: limits };
        let opts = ResolveOptions {
            config: Some(c),
            no_wait: true,
            ..Default::default()
        };
        let err = resolve_agent(opts, &mut probe)
            .await
            .expect_err("should fail");
        assert!(matches!(err, ResolveError::AllLimited(_)));
    }

    #[test]
    fn codexbar_provider_name_aliases_claude_terminal_and_headless() {
        // claude-terminal and claude-headless share the `claude` codexbar account.
        assert_eq!(
            codexbar_provider_name("claude-terminal", "claude"),
            "claude"
        );
        assert_eq!(
            codexbar_provider_name("claude-headless", "claude"),
            "claude"
        );
        // Any other sdk passes the provider name through unchanged.
        assert_eq!(codexbar_provider_name("pi", "zai"), "zai");
        assert_eq!(codexbar_provider_name("pi", "codex"), "codex");
    }

    // -----------------------------------------------------------------------
    // sdk-filter (non-pi providers are excluded from candidates)
    // -----------------------------------------------------------------------

    fn entry_with_sdk(
        key: &str,
        provider: &str,
        sdk: &str,
        models: &[(&str, &str, Option<i32>)],
    ) -> ProviderEntry {
        let mut e = entry(key, provider, None, models);
        e.sdk = sdk.to_string();
        e
    }

    #[test]
    fn build_candidates_filters_out_unsupported_sdks() {
        let c = cfg(vec![
            entry_with_sdk("claude", "claude", "claude", &[("build", "opus", None)]),
            entry_with_sdk("zai", "zai", "pi", &[("build", "anthropic/zai", None)]),
            entry_with_sdk("codex", "codex", "codex", &[("build", "gpt", None)]),
        ]);
        let candidates = build_candidates(&c, "build", None, &[]);
        let providers: Vec<&str> = candidates
            .iter()
            .map(|c| c.resolved.provider.as_str())
            .collect();
        assert_eq!(providers, vec!["zai"]);
    }

    #[test]
    fn unsupported_sdk_providers_lists_non_pi_entries() {
        let c = cfg(vec![
            entry_with_sdk("claude", "claude", "claude", &[("build", "opus", None)]),
            entry_with_sdk("zai", "zai", "pi", &[("build", "z", None)]),
            entry_with_sdk("codex", "codex", "codex", &[("build", "gpt", None)]),
            entry_with_sdk("copilot", "copilot", "copilot", &[("build", "x", None)]),
        ]);
        let mut list = unsupported_sdk_providers(&c);
        list.sort();
        assert_eq!(
            list,
            vec![
                ("claude".to_string(), "claude".to_string()),
                ("codex".to_string(), "codex".to_string()),
                ("copilot".to_string(), "copilot".to_string()),
            ],
        );
    }

    #[test]
    fn unsupported_sdk_providers_empty_when_all_pi() {
        let c = cfg(vec![
            entry_with_sdk("a", "a", "pi", &[("build", "anthropic/x", None)]),
            entry_with_sdk("b", "b", "pi", &[("build", "openai/y", None)]),
        ]);
        assert!(unsupported_sdk_providers(&c).is_empty());
    }

    #[test]
    fn is_supported_sdk_accepts_pi_and_claude_terminal_and_headless() {
        assert!(is_supported_sdk("pi"));
        assert!(is_supported_sdk("claude-terminal"));
        assert!(is_supported_sdk("claude-headless"));
        assert!(!is_supported_sdk("claude"));
        assert!(!is_supported_sdk("codex"));
        assert!(!is_supported_sdk(""));
    }

    // -----------------------------------------------------------------------
    // require_tools (custom tools restrict candidates to the pi sdk)
    // -----------------------------------------------------------------------

    #[test]
    fn sdk_supports_tools_only_pi() {
        assert!(sdk_supports_tools("pi"));
        assert!(!sdk_supports_tools("claude-terminal"));
        assert!(!sdk_supports_tools("claude-headless"));
        assert!(!sdk_supports_tools(""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn require_tools_drops_claude_terminal_candidates() {
        // claude-terminal comes first in config order (higher precedence at equal
        // priority) but must be skipped when tools are required.
        let c = cfg(vec![
            entry_with_sdk(
                "claude",
                "claude",
                "claude-terminal",
                &[("build", "opus", None)],
            ),
            entry_with_sdk("zai", "zai", "pi", &[("build", "anthropic/zai", None)]),
        ]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = ResolveOptions {
            config: Some(c),
            require_tools: true,
            ..Default::default()
        };
        let resolved = resolve_agent(opts, &mut probe).await.expect("resolve");
        assert_eq!(resolved.provider, "zai");
        assert_eq!(resolved.sdk, "pi");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn require_tools_errors_when_only_claude_terminal() {
        let c = cfg(vec![entry_with_sdk(
            "claude",
            "claude",
            "claude-terminal",
            &[("build", "opus", None)],
        )]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = ResolveOptions {
            config: Some(c),
            require_tools: true,
            ..Default::default()
        };
        let err = resolve_agent(opts, &mut probe)
            .await
            .expect_err("should fail");
        assert!(matches!(err, ResolveError::NoMatching(_)), "got: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("claude-terminal"), "got: {msg}");
        assert!(msg.contains("custom tools"), "got: {msg}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn require_tools_false_keeps_claude_terminal() {
        let c = cfg(vec![
            entry_with_sdk(
                "claude",
                "claude",
                "claude-terminal",
                &[("build", "opus", None)],
            ),
            entry_with_sdk("zai", "zai", "pi", &[("build", "anthropic/zai", None)]),
        ]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = ResolveOptions {
            config: Some(c),
            ..Default::default()
        };
        let resolved = resolve_agent(opts, &mut probe).await.expect("resolve");
        assert_eq!(resolved.provider, "claude");
        assert_eq!(resolved.sdk, "claude-terminal");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poll_for_agent_require_tools_drops_claude_terminal() {
        let c = cfg(vec![
            entry_with_sdk(
                "claude",
                "claude",
                "claude-terminal",
                &[("build", "opus", None)],
            ),
            entry_with_sdk("zai", "zai", "pi", &[("build", "anthropic/zai", None)]),
        ]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = PollOptions {
            config: Some(c),
            require_tools: true,
            ..Default::default()
        };
        let resolved = poll_for_agent(opts, &mut probe).await.expect("ok");
        assert_eq!(resolved.provider, "zai");
    }

    // -----------------------------------------------------------------------
    // poll_for_agent cancel-signal handling
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn poll_for_agent_returns_canceled_when_signal_preflipped() {
        // cancel is already true before the first poll iteration → must short-circuit.
        let c = cfg(vec![entry("a", "a", Some(1), &[("build", "x", None)])]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = PollOptions {
            config: Some(c),
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..Default::default()
        };
        let err = poll_for_agent(opts, &mut probe)
            .await
            .expect_err("should be canceled");
        assert!(matches!(err, ResolveError::Canceled), "got: {err:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poll_for_agent_returns_available_when_provider_clear() {
        let c = cfg(vec![entry("a", "a", Some(1), &[("build", "x", None)])]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = PollOptions {
            config: Some(c),
            ..Default::default()
        };
        let resolved = poll_for_agent(opts, &mut probe).await.expect("ok");
        assert_eq!(resolved.provider, "a");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poll_for_agent_no_matching_when_provider_filter_misses() {
        let c = cfg(vec![entry("a", "a", Some(1), &[("build", "x", None)])]);
        let mut probe = MockProbe {
            outcomes: HashMap::new(),
        };
        let opts = PollOptions {
            config: Some(c),
            provider_filter: Some("nope".to_string()),
            ..Default::default()
        };
        let err = poll_for_agent(opts, &mut probe)
            .await
            .expect_err("should fail");
        assert!(matches!(err, ResolveError::NoMatching(_)), "got: {err:?}");
    }

    // -----------------------------------------------------------------------
    // scan probe-error propagation
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn scan_no_agents_carries_probe_error_messages() {
        struct AlwaysErr;
        impl LimitProbe for AlwaysErr {
            fn probe<'a>(
                &'a mut self,
                entry: &'a ProviderEntry,
                _resolved: &'a ResolvedAgent,
            ) -> ProbeFuture<'a> {
                let p = entry.provider.clone();
                Box::pin(async move {
                    let msg: Box<dyn std::error::Error> = format!("boom: {p}").into();
                    Err(msg)
                })
            }
        }

        let c = cfg(vec![
            entry("a", "a", Some(1), &[("build", "x", None)]),
            entry("b", "b", Some(2), &[("build", "y", None)]),
        ]);
        let candidates = build_candidates(&c, "build", None, &[]);
        let mut probe = AlwaysErr;
        let outcome = scan(&candidates, &c.providers, &mut probe).await;
        match outcome {
            ScanOutcome::NoAgents { probe_errors } => {
                assert_eq!(probe_errors.len(), 2);
                assert!(
                    probe_errors
                        .iter()
                        .any(|(p, m)| p == "a" && m.contains("boom: a"))
                );
                assert!(
                    probe_errors
                        .iter()
                        .any(|(p, m)| p == "b" && m.contains("boom: b"))
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_agent_surfaces_probe_errors_in_message() {
        struct AlwaysErr;
        impl LimitProbe for AlwaysErr {
            fn probe<'a>(
                &'a mut self,
                _entry: &'a ProviderEntry,
                _resolved: &'a ResolvedAgent,
            ) -> ProbeFuture<'a> {
                Box::pin(async move {
                    let msg: Box<dyn std::error::Error> = "cookie read failed".into();
                    Err(msg)
                })
            }
        }

        let c = cfg(vec![entry("a", "a", Some(1), &[("build", "x", None)])]);
        let mut probe = AlwaysErr;
        let opts = ResolveOptions {
            config: Some(c),
            ..Default::default()
        };
        let err = resolve_agent(opts, &mut probe)
            .await
            .expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("cookie read failed"), "got: {msg}");
        assert!(msg.contains("probe failures"), "got: {msg}");
    }
}
