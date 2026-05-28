pub mod config;
pub mod config_loader;
pub mod errors;
pub mod pi_runner;
pub mod sleep;
pub mod tools;

#[cfg(feature = "browser")]
pub mod cookies;
#[cfg(feature = "browser")]
pub mod resolve;

pub use config::{
    Config, ModelEntry, ProviderApi, ProviderEntry, ResolvedAgent, ResolvedSkillsConfig,
    SkillsConfig,
};
pub use config_loader::{ConfigError, load_config, resolve_config_path};
pub use errors::{LimitError, RunError, TimeoutError};
pub use pi_runner::{PiRunner, PiRunnerOptions, StreamChunk};
pub use tools::{SeherTool, SeherToolFactory, ToolHandler, make_factory};

#[cfg(feature = "browser")]
pub use cookies::{BrowserSession, provider_to_domain};
#[cfg(feature = "browser")]
pub use resolve::{
    AllAgentsLimitedError, Candidate, CookieProbe, LimitProbe, NoMatchingAgentError, PollOptions,
    ProbeFuture, ResolveError, ResolveOptions, ScanOutcome, alias_limit_provider, build_candidates,
    poll_for_agent, resolve_agent, resolve_agent_with_cookies, scan,
};
