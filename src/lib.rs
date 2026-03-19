#[cfg(feature = "browser")]
pub mod agent;
#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "browser")]
pub mod codex;
#[cfg(feature = "browser")]
pub mod config;
#[cfg(feature = "browser")]
pub mod crypto;

// 常に利用可能（ライブラリとしての公開API）
pub mod claude;
pub mod copilot;
pub mod openrouter;

#[cfg(feature = "browser")]
pub use agent::{Agent, AgentLimit, AgentStatus, UsageEntry};
#[cfg(feature = "browser")]
pub use browser::{BrowserDetector, BrowserType, Cookie, CookieReader, Profile};
pub use claude::{ClaudeClient, UsageResponse, UsageWindow};
#[cfg(feature = "browser")]
pub use codex::{CodexClient, CodexRateLimit, CodexUsageResponse, CodexWindow};
#[cfg(feature = "browser")]
pub use config::{AgentConfig, PriorityRule, Settings};
