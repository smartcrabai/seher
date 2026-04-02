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

// Always available (public API as a library)
pub mod claude;
pub mod copilot;
pub mod glm;
pub mod kimik2;
pub mod kiro;
#[cfg(feature = "browser")]
pub mod opencode_go;
pub mod openrouter;
pub mod warp;
pub mod web;
pub mod zai;

#[cfg(feature = "browser")]
pub use agent::{Agent, AgentLimit, AgentStatus, UsageEntry};
#[cfg(feature = "browser")]
pub use browser::{BrowserDetector, BrowserType, Cookie, CookieReader, Profile};
pub use claude::{ClaudeClient, UsageResponse, UsageWindow};
#[cfg(feature = "browser")]
pub use codex::{CodexClient, CodexRateLimit, CodexUsageResponse, CodexWindow};
#[cfg(feature = "browser")]
pub use config::{AgentConfig, PriorityRule, Settings};
#[cfg(feature = "browser")]
pub use opencode_go::{OpencodeGoAuth, OpencodeGoUsageSnapshot, OpencodeGoUsageStore};
