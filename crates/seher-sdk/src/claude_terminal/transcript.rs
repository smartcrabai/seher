use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::{
    ClaudeSessionRef, ClaudeTerminalError, ClaudeTerminalResponse, ClaudeTranscriptReader,
    FindClaudeSessionOptions, TranscriptMessage, WaitForAssistantResponseOptions,
};

/// Claude Code encodes the cwd into the directory name under `~/.claude/projects/`.
/// Every character that is not an ASCII letter, digit, or hyphen is replaced with `-`.
#[must_use]
pub fn encode_project_dir(cwd: &str) -> String {
    // canonicalize so relative paths are resolved before encoding
    let path = std::fs::canonicalize(cwd).unwrap_or_else(|_| {
        std::env::current_dir().map_or_else(|_| PathBuf::from(cwd), |base| base.join(cwd))
    });
    path.to_string_lossy()
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

#[must_use]
pub fn default_transcript_root() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
        .to_string_lossy()
        .into_owned()
}

fn now_ms() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(dur).unwrap_or(u64::MAX)
}

/// Parse an ISO-8601 / RFC-3339 transcript timestamp (e.g. `2026-06-01T21:57:49.050Z`)
/// into epoch milliseconds.
fn ts_to_ms(ts: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .and_then(|ms| u64::try_from(ms).ok())
}

/// Whether a transcript message belongs to the current turn (timestamp at or after the
/// `after_ms` cutoff). Untimestamped lines are conservatively kept — in real Claude
/// transcripts the scanned line types (`assistant`/`user`/`system`) always carry one.
fn msg_in_window(m: &TranscriptMessage, after_ms: u64) -> bool {
    match m.timestamp.as_deref().and_then(ts_to_ms) {
        Some(ms) => ms >= after_ms,
        None => true,
    }
}

/// Stricter window check for turn-completion markers (`result` / `turn_duration`): with
/// a cutoff in effect, an untimestamped marker cannot be proven to belong to the current
/// turn, and accepting a stale one would end the wait with a previous turn's output.
fn marker_in_window(m: &TranscriptMessage, after_ms: u64) -> bool {
    after_ms == 0
        || m.timestamp
            .as_deref()
            .and_then(ts_to_ms)
            .is_some_and(|ms| ms >= after_ms)
}

fn has_jsonl_extension(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

fn project_dir(root: &str, cwd: &str) -> PathBuf {
    PathBuf::from(root).join(encode_project_dir(cwd))
}

// ── FileSystemTranscriptReader ───────────────────────────────────────────────

#[derive(Default)]
pub struct FileSystemTranscriptReader;

impl FileSystemTranscriptReader {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ClaudeTranscriptReader for FileSystemTranscriptReader {
    fn list_session_names(
        &self,
        root: &str,
        cwd: &str,
    ) -> Result<HashSet<String>, ClaudeTerminalError> {
        let dir = project_dir(root, cwd);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(HashSet::new());
        };
        Ok(entries
            .filter_map(|e| e.ok()?.file_name().to_str().map(str::to_string))
            .filter(|n| has_jsonl_extension(n))
            .collect())
    }

    fn find_session(
        &self,
        options: FindClaudeSessionOptions,
    ) -> Result<ClaudeSessionRef, ClaudeTerminalError> {
        let dir = project_dir(&options.root, &options.cwd);
        let deadline = now_ms().saturating_add(options.timeout_ms);
        loop {
            let entries: Vec<String> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok()?.file_name().to_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            let mut candidates: Vec<(PathBuf, u64)> = entries
                .into_iter()
                .filter(|name| has_jsonl_extension(name) && !options.exclude_names.contains(name))
                .filter_map(|name| {
                    let path = dir.join(&name);
                    let mtime = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis())
                        .and_then(|ms| u64::try_from(ms).ok())?;
                    if mtime >= options.after_ms {
                        Some((path, mtime))
                    } else {
                        None
                    }
                })
                .collect();

            candidates.sort_by_key(|(_, mtime)| *mtime);

            if let Some((path, _)) = candidates.into_iter().next() {
                let session_id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                return Ok(ClaudeSessionRef {
                    session_id,
                    transcript_path: path.to_string_lossy().into_owned(),
                });
            }
            if now_ms() >= deadline {
                return Err(ClaudeTerminalError::Timeout(format!(
                    "timed out finding Claude transcript under {}",
                    dir.display()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(options.poll_interval_ms));
        }
    }

    fn wait_for_assistant_response(
        &self,
        session: &ClaudeSessionRef,
        options: WaitForAssistantResponseOptions,
    ) -> Result<ClaudeTerminalResponse, ClaudeTerminalError> {
        let deadline = now_ms().saturating_add(options.timeout_ms);
        loop {
            let raw = std::fs::read_to_string(&session.transcript_path).unwrap_or_default();
            let messages = parse_jsonl(&raw);
            let scan = scan_transcript(&messages, options.after_ms);
            if scan.last_result.is_some() {
                return Ok(ClaudeTerminalResponse {
                    session_id: session.session_id.clone(),
                    assistant_messages: scan.assistant_messages,
                    last_result_message: scan.last_result,
                });
            }
            if scan.turn_complete && !scan.assistant_messages.is_empty() {
                return Ok(ClaudeTerminalResponse {
                    session_id: session.session_id.clone(),
                    assistant_messages: scan.assistant_messages,
                    last_result_message: None,
                });
            }
            if now_ms() >= deadline {
                if !scan.assistant_messages.is_empty() {
                    return Ok(ClaudeTerminalResponse {
                        session_id: session.session_id.clone(),
                        assistant_messages: scan.assistant_messages,
                        last_result_message: None,
                    });
                }
                return Err(ClaudeTerminalError::Timeout(format!(
                    "timed out waiting for Claude assistant response in {}",
                    session.transcript_path
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(options.poll_interval_ms));
        }
    }
}

struct TranscriptScan {
    assistant_messages: Vec<TranscriptMessage>,
    last_result: Option<TranscriptMessage>,
    turn_complete: bool,
}

fn scan_transcript(messages: &[TranscriptMessage], after_ms: u64) -> TranscriptScan {
    let mut assistant_messages = Vec::new();
    let mut last_result = None;
    let mut turn_complete = false;
    for m in messages {
        // Skip messages from prior turns (relevant when resuming an existing transcript).
        if !msg_in_window(m, after_ms) {
            continue;
        }
        match m.msg_type.as_str() {
            "assistant" => assistant_messages.push(m.clone()),
            "result" if marker_in_window(m, after_ms) => last_result = Some(m.clone()),
            "system"
                if m.subtype.as_deref() == Some("turn_duration")
                    && marker_in_window(m, after_ms) =>
            {
                turn_complete = true;
            }
            _ => {}
        }
    }
    TranscriptScan {
        assistant_messages,
        last_result,
        turn_complete,
    }
}

#[must_use]
pub fn parse_jsonl(raw: &str) -> Vec<TranscriptMessage> {
    let valid_types = ["assistant", "user", "result", "system"];
    raw.lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let t = v.get("type")?.as_str()?;
            if !valid_types.contains(&t) {
                return None;
            }
            serde_json::from_value(v).ok()
        })
        .collect()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    #[test]
    fn encode_project_dir_replaces_non_alnum() {
        // Use a real absolute path so canonicalize succeeds on macOS CI
        let encoded = encode_project_dir("/tmp");
        assert!(!encoded.contains('/'), "slashes should be replaced");
    }

    #[test]
    fn encode_project_dir_dot_becomes_dash() {
        // Build a path under /tmp which always exists
        let encoded = encode_project_dir("/tmp/.seher-test");
        assert!(!encoded.contains('.'), "dots should be replaced");
        assert!(!encoded.contains('/'), "slashes should be replaced");
    }

    #[test]
    fn parse_jsonl_skips_invalid_lines() {
        let raw = r#"{"type":"assistant","message":{"content":"hi"}}
not-json
{"type":"unknown_type"}
{"type":"result","result":"done"}"#;
        let msgs = parse_jsonl(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].msg_type, "assistant");
        assert_eq!(msgs[1].msg_type, "result");
    }

    #[test]
    fn scan_transcript_detects_turn_complete() {
        let raw = r#"{"type":"assistant","message":{"content":"hello"}}
{"type":"system","subtype":"turn_duration"}"#;
        let msgs = parse_jsonl(raw);
        let scan = scan_transcript(&msgs, 0);
        assert_eq!(scan.assistant_messages.len(), 1);
        assert!(scan.turn_complete);
        assert!(scan.last_result.is_none());
    }

    #[test]
    fn scan_transcript_detects_result() {
        let raw = r#"{"type":"assistant","message":{"content":"hello"}}
{"type":"result","result":"final answer"}"#;
        let msgs = parse_jsonl(raw);
        let scan = scan_transcript(&msgs, 0);
        assert!(scan.last_result.is_some());
        assert_eq!(
            scan.last_result.unwrap().result.as_deref(),
            Some("final answer")
        );
    }

    #[test]
    fn scan_transcript_excludes_messages_before_cutoff() {
        // Simulates a resumed transcript: an older turn (and its turn_duration) sits
        // before the cutoff, the current turn sits after it.
        let raw = r#"{"type":"assistant","message":{"content":"old"},"timestamp":"2026-06-01T00:00:00.000Z"}
{"type":"system","subtype":"turn_duration","timestamp":"2026-06-01T00:00:01.000Z"}
{"type":"assistant","message":{"content":"new"},"timestamp":"2026-06-01T12:00:00.000Z"}"#;
        let msgs = parse_jsonl(raw);
        // Cutoff at 2026-06-01T06:00:00Z (epoch ms) — only the "new" assistant message qualifies.
        let cutoff = ts_to_ms("2026-06-01T06:00:00.000Z").unwrap();
        let scan = scan_transcript(&msgs, cutoff);
        assert_eq!(
            scan.assistant_messages.len(),
            1,
            "only the post-cutoff turn"
        );
        assert!(
            !scan.turn_complete,
            "the pre-cutoff turn_duration must not count as this turn completing"
        );
    }

    #[test]
    fn ts_to_ms_parses_iso8601() {
        assert!(ts_to_ms("2026-06-01T21:57:49.050Z").is_some());
        assert!(ts_to_ms("not-a-timestamp").is_none());
    }

    #[test]
    fn scan_transcript_ignores_untimestamped_markers_when_cutoff_active() {
        // A resumed transcript may contain completion markers without timestamps; with a
        // cutoff they must not terminate the wait (they could be from a prior turn).
        let raw = r#"{"type":"result","result":"stale answer"}
{"type":"system","subtype":"turn_duration"}"#;
        let msgs = parse_jsonl(raw);
        let scan = scan_transcript(&msgs, 1);
        assert!(
            scan.last_result.is_none(),
            "untimestamped result must not count"
        );
        assert!(
            !scan.turn_complete,
            "untimestamped turn_duration must not count"
        );
        // Without a cutoff (fresh transcript semantics) they still count.
        let scan = scan_transcript(&msgs, 0);
        assert!(scan.last_result.is_some());
        assert!(scan.turn_complete);
    }
}
