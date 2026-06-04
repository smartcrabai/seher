//! `pi_agent_rust` bridge.
//!
//! Runs prompts on a dedicated `std::thread` driven by `futures::executor::block_on`
//! to avoid nested-runtime panics when the caller is also driving a tokio runtime
//! for cookie-based limit checks.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::sdk::errors::{LimitError, RunError};
use crate::sdk::tool::{PiToolAdapter, SeherTool};

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
    /// The backend session id for this run (newly generated or resumed). Emitted once,
    /// before any `Delta`, so consumers can persist it for a follow-up turn.
    Session(String),
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
    /// Thinking level passed to pi (e.g. `"off"`, `"low"`, `"medium"`, `"high"`).
    /// Parsed with [`pi::model::ThinkingLevel::FromStr`]; an invalid value fails
    /// the run fast with [`StreamChunk::Error`]. `None` keeps pi's default.
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
    /// Working directory the agent operates in. Also binds where multi-turn session
    /// files live (see [`pi_session_path`]).
    pub working_directory: Option<PathBuf>,
    /// Custom tools (function calling) injected into the agent session before the
    /// prompt runs. Empty = no custom tools (pi's built-in tools still apply).
    ///
    /// Names must be unique and must not collide with pi's built-in tools
    /// (the run fails fast with [`StreamChunk::Error`] otherwise). When passing
    /// tools, resolve the agent with `ResolveOptions::require_tools` so resolution
    /// never selects `claude-terminal`, which would silently ignore them.
    pub tools: Vec<SeherTool>,
}

impl std::fmt::Debug for PiRunnerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiRunnerOptions")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("thinking", &self.thinking)
            .field("system_prompt", &self.system_prompt)
            .field("working_directory", &self.working_directory)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Output of a completed [`PiRunner::run`]: the full assistant text plus the
/// session id (newly generated for a fresh run, or the resumed id).
#[derive(Debug, Clone)]
pub struct PiRunOutput {
    pub text: String,
    pub session_id: String,
}

/// Encode `cwd` into a filesystem-safe directory name (every non `[A-Za-z0-9-]`
/// char becomes `-`), mirroring how Claude Code names its project transcript dirs.
fn encode_cwd_dir(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Deterministic on-disk path for a pi multi-turn session, bound to `working_directory`
/// (defaults to the process cwd) and the session `id`. Same `(cwd, id)` always maps to
/// the same `.jsonl` file, so a follow-up turn resumes the prior conversation.
#[must_use]
pub fn pi_session_path(working_directory: Option<&Path>, id: &str) -> PathBuf {
    let cwd = working_directory.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        Path::to_path_buf,
    );
    // Canonicalize so symlinked/relative forms of the same directory encode identically
    // (mirrors claude_terminal::encode_project_dir) — otherwise a session written from a
    // non-canonical cwd could not be found when probed with the canonical one.
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let base = dirs::data_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
        .join("seher")
        .join("pi-sessions");
    base.join(encode_cwd_dir(&cwd)).join(format!("{id}.jsonl"))
}

/// Seed a fresh, header-only session file that pi's `Session::open` accepts. pi errors
/// with `SessionNotFound` when `session_path` points at a missing file, so a new
/// conversation must create it up front. The header is built by pi itself
/// ([`pi::session::SessionHeader::new`]) so the on-disk format/version always matches the
/// linked pi crate; only the id (seher-owned) and cwd are overridden.
fn seed_session_file(
    path: &Path,
    id: &str,
    working_directory: Option<&Path>,
) -> std::io::Result<()> {
    let mut header = pi::session::SessionHeader::new();
    header.id = id.to_string();
    if let Some(cwd) = working_directory {
        header.cwd = cwd.display().to_string();
    }
    let line = serde_json::to_string(&header).map_err(std::io::Error::other)?;
    std::fs::write(path, format!("{line}\n"))
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
    ///
    /// `resume` is the session id of a prior turn to continue; `None` starts a fresh
    /// session (and a new id is generated). The session id for this run is emitted as
    /// the first chunk via [`StreamChunk::Session`].
    #[must_use]
    pub fn stream(&self, prompt: String, resume: Option<String>) -> Receiver<StreamChunk> {
        let (tx, rx) = channel();
        let opts = self.opts.clone();
        thread::spawn(move || run_on_thread(&opts, &prompt, resume.as_deref(), &tx));
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
    pub fn run(&self, prompt: String, resume: Option<String>) -> Result<PiRunOutput, RunError> {
        let rx = self.stream(prompt, resume);
        let mut buffered = String::new();
        let mut session_id = String::new();
        loop {
            match rx.recv() {
                Ok(StreamChunk::Delta(d)) => buffered.push_str(&d),
                Ok(StreamChunk::Session(id)) => session_id = id,
                Ok(StreamChunk::Done(text)) => {
                    return Ok(PiRunOutput {
                        text: if text.is_empty() { buffered } else { text },
                        session_id,
                    });
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

/// Validate custom tool names: a name colliding with a pi built-in would be
/// silently shadowed at dispatch (pi's registry returns the first match), and
/// duplicate names produce duplicate tool definitions that providers reject.
fn validate_tool_names(tools: &[SeherTool]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        if pi::sdk::BUILTIN_TOOL_NAMES.contains(&tool.name.as_str()) {
            return Err(format!(
                "custom tool '{}' collides with a pi built-in tool ({})",
                tool.name,
                pi::sdk::BUILTIN_TOOL_NAMES.join(", ")
            ));
        }
        if !seen.insert(tool.name.as_str()) {
            return Err(format!("duplicate custom tool name '{}'", tool.name));
        }
    }
    Ok(())
}

/// Parses an optional thinking-level string into pi's [`pi::model::ThinkingLevel`].
/// `None` stays `None` (pi's default); an unrecognized value is an error.
/// Splits a trailing `:level` thinking suffix off a model name, returning
/// `(model, thinking)`.
///
/// The suffix is recognized **only** when it parses as a pi thinking level
/// (`off`, `minimal`, `low`, `medium`, `high`, `xhigh` and their aliases —
/// see [`pi::model::ThinkingLevel`]'s `FromStr`). Anything else stays part of
/// the model name, so model ids with a legitimate `:` — e.g. `OpenRouter`
/// variants like `meta-llama/llama-3.1-8b-instruct:free` — pass through
/// untouched.
#[must_use]
pub fn split_thinking_suffix(model: &str) -> (&str, Option<&str>) {
    match model.rsplit_once(':') {
        Some((m, t)) if t.parse::<pi::model::ThinkingLevel>().is_ok() => (m, Some(t)),
        _ => (model, None),
    }
}

fn parse_thinking(thinking: Option<&str>) -> Result<Option<pi::model::ThinkingLevel>, String> {
    thinking
        .map(|t| {
            t.parse::<pi::model::ThinkingLevel>()
                .map_err(|e| format!("invalid thinking level '{t}': {e}"))
        })
        .transpose()
}

fn run_on_thread(
    opts: &PiRunnerOptions,
    prompt: &str,
    resume: Option<&str>,
    tx: &Sender<StreamChunk>,
) {
    use pi::model::AssistantMessageEvent;
    use pi::sdk::{AgentEvent, SessionOptions, create_agent_session};

    // Fail fast on invalid tool names, before any session file is created.
    if let Err(msg) = validate_tool_names(&opts.tools) {
        let _ = tx.send(StreamChunk::Error(msg));
        return;
    }

    // Same fail-fast treatment for an invalid thinking level.
    let thinking = match parse_thinking(opts.thinking.as_deref()) {
        Ok(t) => t,
        Err(msg) => {
            let _ = tx.send(StreamChunk::Error(msg));
            return;
        }
    };

    // Multi-turn: seher owns the session id. `resume` continues a prior turn; otherwise
    // a fresh v4 uuid is generated. The id maps to a deterministic on-disk session file
    // bound to the working directory, so the next turn can resume by passing it back.
    let session_id = resume.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
    let session_path = pi_session_path(opts.working_directory.as_deref(), &session_id);

    // A fresh conversation must seed the session file before create_agent_session —
    // pi only opens existing files at `session_path` (a resumed one already exists).
    if resume.is_none() {
        let created = session_path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                seed_session_file(
                    &session_path,
                    &session_id,
                    opts.working_directory.as_deref(),
                )
            });
        if let Err(e) = created {
            let _ = tx.send(StreamChunk::Error(format!(
                "failed to create session file {}: {e}",
                session_path.display()
            )));
            return;
        }
    }

    // Surface the session id up front so a streaming consumer can persist it even if
    // the turn later errors out.
    let _ = tx.send(StreamChunk::Session(session_id.clone()));

    // The system prompt is applied as the session's system prompt (not concatenated into
    // the user turn), so multi-turn conversations stay clean. pi does not persist the
    // system prompt in the session file, so we pass it on every turn (resume included).
    let prompt_text = prompt.to_string();

    let provider_label = opts.provider.clone().unwrap_or_else(|| "pi".to_string());

    let outcome: Result<(), CloseOutcome> = futures::executor::block_on(async {
        let session_opts = SessionOptions {
            provider: opts.provider.clone(),
            model: opts.model.clone(),
            api_key: opts.api_key.clone(),
            thinking,
            system_prompt: opts.system_prompt.clone(),
            working_directory: opts.working_directory.clone(),
            no_session: false,
            session_path: Some(session_path.clone()),
            ..Default::default()
        };

        let mut handle = create_agent_session(session_opts)
            .await
            .map_err(|e| CloseOutcome::Error(format!("create_agent_session failed: {e}")))?;

        // Inject custom tools into the live agent before prompting. SessionOptions
        // has no tool-injection field, so we reach the Agent via the session handle.
        // Must happen before `prompt` so the tool definitions reach the provider.
        if !opts.tools.is_empty() {
            let custom: Vec<Box<dyn pi::tools::Tool>> = opts
                .tools
                .iter()
                .cloned()
                .map(|t| Box::new(PiToolAdapter::new(t)) as Box<dyn pi::tools::Tool>)
                .collect();
            handle.session_mut().agent.extend_tools(custom);
        }

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
            let _ = tx.send(StreamChunk::Done(String::new()));
        }
        Err(CloseOutcome::Limit(e)) => {
            let _ = tx.send(StreamChunk::Limit(e));
        }
        Err(CloseOutcome::Error(msg)) => {
            let _ = tx.send(StreamChunk::Error(msg));
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
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
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
    fn parse_thinking_accepts_known_levels() {
        assert_eq!(parse_thinking(None), Ok(None));
        assert_eq!(
            parse_thinking(Some("high")),
            Ok(Some(pi::model::ThinkingLevel::High))
        );
        assert_eq!(
            parse_thinking(Some("off")),
            Ok(Some(pi::model::ThinkingLevel::Off))
        );
    }

    #[test]
    fn split_thinking_suffix_extracts_known_levels() {
        assert_eq!(
            split_thinking_suffix("opus-4.7:high"),
            ("opus-4.7", Some("high"))
        );
        // FromStr aliases are recognized too.
        assert_eq!(
            split_thinking_suffix("opus-4.7:med"),
            ("opus-4.7", Some("med"))
        );
        // Only the last `:` is considered; earlier ones stay in the model name.
        assert_eq!(
            split_thinking_suffix("llama-3.1:free:low"),
            ("llama-3.1:free", Some("low"))
        );
    }

    #[test]
    fn split_thinking_suffix_keeps_unrecognized_suffix() {
        assert_eq!(
            split_thinking_suffix("meta-llama/llama-3.1-8b-instruct:free"),
            ("meta-llama/llama-3.1-8b-instruct:free", None)
        );
        assert_eq!(split_thinking_suffix("opus-4.7"), ("opus-4.7", None));
        // A trailing empty suffix is not a level; left untouched.
        assert_eq!(split_thinking_suffix("opus-4.7:"), ("opus-4.7:", None));
    }

    #[test]
    fn parse_thinking_rejects_unknown_level() {
        let err = parse_thinking(Some("turbo")).expect_err("'turbo' must not parse");
        assert!(err.contains("invalid thinking level 'turbo'"), "{err}");
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

    /// Contract with pi: a fresh session file seeded by [`seed_session_file`] must be
    /// accepted by `Session::open` (a new conversation passes `session_path` pointing
    /// at it). Guards against pi on-disk format drift.
    #[test]
    #[expect(clippy::unwrap_used, reason = "test panics on unexpected fixtures")]
    fn seeded_session_file_is_openable_by_pi() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seeded.jsonl");
        let id = "11111111-2222-3333-4444-555555555555";
        seed_session_file(&path, id, Some(dir.path())).unwrap();

        let session =
            futures::executor::block_on(pi::session::Session::open(&path.to_string_lossy()))
                .unwrap();
        assert_eq!(session.header.id, id);
        assert_eq!(session.header.cwd, dir.path().display().to_string());
        assert!(session.entries.is_empty());
    }

    #[test]
    fn pi_runner_options_default_has_no_tools() {
        assert!(PiRunnerOptions::default().tools.is_empty());
    }

    #[test]
    fn pi_runner_options_debug_masks_api_key_and_lists_tool_names() {
        let opts = PiRunnerOptions {
            api_key: Some("sk-secret".to_string()),
            tools: vec![SeherTool::new(
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
                std::sync::Arc::new(|_| Ok(String::new())),
            )],
            ..Default::default()
        };
        let dbg = format!("{opts:?}");
        assert!(!dbg.contains("sk-secret"), "got: {dbg}");
        assert!(dbg.contains("***"), "got: {dbg}");
        assert!(dbg.contains("echo"), "got: {dbg}");
    }

    fn named_tool(name: &str) -> SeherTool {
        SeherTool::new(
            name,
            "test tool",
            serde_json::json!({"type": "object"}),
            std::sync::Arc::new(|_| Ok(String::new())),
        )
    }

    #[test]
    fn validate_tool_names_accepts_unique_custom_names() {
        let tools = vec![named_tool("alpha"), named_tool("beta")];
        assert!(validate_tool_names(&tools).is_ok());
        assert!(validate_tool_names(&[]).is_ok());
    }

    #[test]
    fn validate_tool_names_rejects_builtin_collision() {
        let err = validate_tool_names(&[named_tool("read")]).expect_err("should reject");
        assert!(err.contains("read"), "got: {err}");
        assert!(err.contains("built-in"), "got: {err}");
    }

    #[test]
    fn validate_tool_names_rejects_duplicates() {
        let err = validate_tool_names(&[named_tool("alpha"), named_tool("alpha")])
            .expect_err("should reject");
        assert!(err.contains("duplicate"), "got: {err}");
        assert!(err.contains("alpha"), "got: {err}");
    }

    #[test]
    fn stream_emits_error_on_builtin_tool_collision() {
        // The full stream path must fail fast (no session file, no pi call) when a
        // custom tool shadows a built-in.
        let runner = PiRunner::new(PiRunnerOptions {
            tools: vec![named_tool("bash")],
            ..Default::default()
        });
        let rx = runner.stream("hi".to_string(), None);
        match rx.recv().expect("one chunk") {
            StreamChunk::Error(msg) => assert!(msg.contains("bash"), "got: {msg}"),
            other => panic!("expected Error chunk, got {other:?}"),
        }
    }

    #[test]
    fn pi_session_path_is_deterministic_for_same_cwd_and_id() {
        let dir = std::env::temp_dir();
        let a = pi_session_path(Some(&dir), "abc");
        let b = pi_session_path(Some(&dir), "abc");
        assert_eq!(a, b);
        assert!(a.to_string_lossy().ends_with("abc.jsonl"));
    }

    #[test]
    fn pi_session_path_canonicalizes_symlinked_cwd() {
        // The canonical and non-canonical forms of the same directory must map to the
        // same session file, or resume probing would miss sessions written via symlinks.
        let dir = std::env::temp_dir();
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        assert_eq!(
            pi_session_path(Some(&dir), "abc"),
            pi_session_path(Some(&canonical), "abc"),
        );
    }
}
