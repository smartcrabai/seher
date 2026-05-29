//! pi_agent_rust bridge.
//!
//! Runs prompts on a dedicated `std::thread` driven by `futures::executor::block_on`
//! to avoid nested-runtime panics when the caller is also driving a tokio runtime
//! for cookie-based limit checks.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use pi::sdk::ToolFactory;

use crate::sdk::errors::{LimitError, RunError};

/// Phrases that indicate the pi error was caused by a rate / usage limit. Matched
/// against tokenized words (alphanumeric + `-`) so substrings like `"5429 bytes"`
/// or `"missing 'quota' field"` do **not** trigger a false positive.
const PI_LIMIT_TOKENS: &[&str] = &[
    "429",
    "ratelimit",
    "rate-limit",
    "rate-limited",
    "usagelimit",
    "usage-limit",
    "usage-limited",
    "quota",
];

/// Phrases matched as a multi-word substring (lowercased).
const PI_LIMIT_PHRASES: &[&str] = &["rate limit", "usage limit", "too many requests"];

fn is_pi_limit(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    if PI_LIMIT_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // Tokenize on whitespace and common punctuation; match tokens exactly.
    lower
        .split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '(' | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                        | ';'
                        | ':'
                        | '.'
                        | '\''
                        | '"'
                        | '/'
                        | '\\'
                        | '!'
                        | '?'
                )
        })
        .filter(|t| !t.is_empty())
        .any(|t| PI_LIMIT_TOKENS.contains(&t))
}

#[derive(Debug)]
pub enum StreamChunk {
    /// Streaming text delta from the assistant.
    Delta(String),
    /// Final completion with the full assistant text (may be empty if all text was already
    /// surfaced via Delta).
    Done(String),
    /// pi returned an error that looked like a rate/usage limit.
    Limit(LimitError),
    /// Any other error (provider error, transport error, …) — stringified.
    Error(String),
}

#[derive(Clone, Default)]
pub struct PiRunnerOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    /// Optional in-process tool factory (see [`crate::sdk::tools`]).
    pub tool_factory: Option<Arc<dyn ToolFactory>>,
}

impl std::fmt::Debug for PiRunnerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiRunnerOptions")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("system_prompt", &self.system_prompt)
            .field("tool_factory", &self.tool_factory.as_ref().map(|_| "<set>"))
            .finish()
    }
}

pub struct PiRunner {
    opts: PiRunnerOptions,
}

impl PiRunner {
    #[must_use]
    pub fn new(opts: PiRunnerOptions) -> Self {
        Self { opts }
    }

    /// Spawn a dedicated worker thread that drives pi via `futures::executor::block_on`,
    /// emitting `StreamChunk`s on the returned channel.
    #[must_use]
    pub fn stream(&self, prompt: String) -> Receiver<StreamChunk> {
        let (tx, rx) = channel();
        let opts = self.opts.clone();
        thread::spawn(move || run_on_thread(opts, prompt, tx));
        rx
    }

    /// Convenience: drain the stream into a single string. Concatenates deltas; if `Done`
    /// carries a non-empty final text it overrides the concatenated deltas.
    ///
    /// On `Limit` / `Error`, the partial text accumulated so far is returned alongside
    /// the error as `RunError::Limit { partial }` / `RunError::Other { partial }`.
    ///
    /// # Errors
    ///
    /// Returns `RunError::Limit` on pi rate/usage limits, `RunError::Other` for any other
    /// failure (transport error, channel disconnect, etc.).
    pub fn run(&self, prompt: String) -> Result<String, RunError> {
        let rx = self.stream(prompt);
        let mut buffered = String::new();
        loop {
            match rx.recv() {
                Ok(StreamChunk::Delta(d)) => buffered.push_str(&d),
                Ok(StreamChunk::Done(text)) => {
                    return Ok(if text.is_empty() { buffered } else { text });
                }
                Ok(StreamChunk::Limit(error)) => {
                    return Err(RunError::Limit {
                        error,
                        partial: buffered,
                    });
                }
                Ok(StreamChunk::Error(msg)) => {
                    return Err(RunError::Other {
                        message: msg,
                        partial: buffered,
                    });
                }
                Err(_) => {
                    return Err(RunError::Other {
                        message: "pi runner channel closed".to_string(),
                        partial: buffered,
                    });
                }
            }
        }
    }
}

fn run_on_thread(opts: PiRunnerOptions, prompt: String, tx: Sender<StreamChunk>) {
    use pi::model::AssistantMessageEvent;
    use pi::sdk::{AgentEvent, SessionOptions, create_agent_session};

    let prompt_text = match opts.system_prompt.as_deref() {
        Some(sys) => format!("{sys}\n\n{prompt}"),
        None => prompt,
    };

    let provider_label = opts.provider.clone().unwrap_or_else(|| "pi".to_string());
    let tx_for_close = tx.clone();

    let outcome: Result<(), CloseOutcome> = futures::executor::block_on(async {
        let mut session_opts = SessionOptions::default();
        session_opts.provider = opts.provider.clone();
        session_opts.model = opts.model.clone();
        session_opts.api_key = opts.api_key.clone();
        session_opts.no_session = true;
        session_opts.tool_factory = opts.tool_factory.clone();

        let mut handle = create_agent_session(session_opts)
            .await
            .map_err(|e| CloseOutcome::Error(format!("create_agent_session failed: {e}")))?;

        let txd = tx.clone();
        handle
            .prompt(&prompt_text, move |ev: AgentEvent| {
                if let AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } = ev
                    && let AssistantMessageEvent::TextDelta { delta, .. } = assistant_message_event
                {
                    let _ = txd.send(StreamChunk::Delta(delta));
                }
            })
            .await
            .map_err(|e| classify_pi_error(&provider_label, &e.to_string()))?;

        Ok(())
    });

    match outcome {
        Ok(()) => {
            let _ = tx_for_close.send(StreamChunk::Done(String::new()));
        }
        Err(CloseOutcome::Limit(e)) => {
            let _ = tx_for_close.send(StreamChunk::Limit(e));
        }
        Err(CloseOutcome::Error(msg)) => {
            let _ = tx_for_close.send(StreamChunk::Error(msg));
        }
    }
}

enum CloseOutcome {
    Limit(LimitError),
    Error(String),
}

fn classify_pi_error(provider: &str, msg: &str) -> CloseOutcome {
    if is_pi_limit(msg) {
        CloseOutcome::Limit(LimitError {
            provider: provider.to_string(),
            reset_at: None,
        })
    } else {
        CloseOutcome::Error(msg.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_limit_phrases() {
        assert!(is_pi_limit("Rate limit exceeded"));
        assert!(is_pi_limit("usage-limit"));
        assert!(is_pi_limit("HTTP 429 Too Many Requests"));
        assert!(is_pi_limit("Quota exceeded for the day"));
    }

    #[test]
    fn rejects_unrelated_messages() {
        assert!(!is_pi_limit("unexpected end of stream"));
        assert!(!is_pi_limit("connection refused"));
    }

    #[test]
    fn rejects_substring_false_positives() {
        // "5429" must not match "429"
        assert!(!is_pi_limit("Read 5429 bytes before EOF"));
        // "quota" inside another word must not match
        assert!(!is_pi_limit("loaded squotahelper module"));
        // bare numeric 429 inside HTTP status text still matches (separated by space)
        assert!(is_pi_limit("status 429 returned"));
    }
}
