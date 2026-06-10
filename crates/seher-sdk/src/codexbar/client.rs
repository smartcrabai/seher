//! Spawns the external `codexbar` binary and parses its JSON usage payload.
//!
//! Mirrors `seher-ts/packages/sdk/src/codexbar/client.ts`. We follow the
//! codebase convention of running blocking `std::process` work on a Tokio
//! blocking thread (see `kiro::client`) rather than pulling in the tokio
//! `process` feature. A hard timeout is enforced by polling `try_wait` and
//! killing the child once the deadline passes.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::errors::CodexBarError;
use super::types::CodexBarUsageResponse;

const DEFAULT_BIN: &str = "/usr/local/bin/codexbar";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// codexbar exits with this code when its own internal usage fetch times out.
const TIMEOUT_EXIT_CODE: i32 = 4;

/// Options for [`run_codexbar_usage`]. Defaults mirror seher-ts.
#[derive(Debug, Clone, Default)]
pub struct RunCodexBarUsageOptions {
    /// Explicit binary path; overrides `SEHER_CODEXBAR_BIN`, PATH lookup and the default.
    pub bin_path: Option<String>,
    /// `--account <label>` selector.
    pub account_label: Option<String>,
    /// `--account-index <n>` selector.
    pub account_index: Option<i64>,
    /// Hard timeout; defaults to 15s.
    pub timeout: Option<Duration>,
}

fn which_codexbar() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("codexbar");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_bin_path(explicit: Option<&str>) -> PathBuf {
    if let Some(p) = explicit
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Ok(env_bin) = std::env::var("SEHER_CODEXBAR_BIN")
        && !env_bin.is_empty()
    {
        return PathBuf::from(env_bin);
    }
    which_codexbar().unwrap_or_else(|| PathBuf::from(DEFAULT_BIN))
}

/// Run `codexbar usage --format json --provider <provider>` and return the entry
/// matching `provider`.
///
/// # Errors
///
/// Returns [`CodexBarError`] when the binary is missing, the process fails or
/// times out, the output is not valid JSON, or no entry matches `provider`.
pub async fn run_codexbar_usage(
    provider: &str,
    opts: &RunCodexBarUsageOptions,
) -> Result<CodexBarUsageResponse, CodexBarError> {
    let bin = resolve_bin_path(opts.bin_path.as_deref());
    let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let provider = provider.to_string();

    let mut args: Vec<String> = vec![
        "usage".into(),
        "--format".into(),
        "json".into(),
        "--provider".into(),
        provider.clone(),
    ];
    if let Some(label) = &opts.account_label {
        args.push("--account".into());
        args.push(label.clone());
    }
    if let Some(idx) = opts.account_index {
        args.push("--account-index".into());
        args.push(idx.to_string());
    }

    let provider_for_blocking = provider.clone();
    let raw = tokio::task::spawn_blocking(move || {
        run_blocking(&bin, &args, timeout, &provider_for_blocking)
    })
    .await
    .map_err(|e| CodexBarError::Spawn(e.to_string()))??;

    parse_response(&raw.stdout, &raw.stderr, raw.code, &provider)
}

struct RawOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// Drain a child pipe to a `String` on its own thread. Returning the reader as a
/// join handle lets the caller consume stdout/stderr concurrently with the
/// process-completion poll loop.
fn spawn_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    })
}

fn run_blocking(
    bin: &Path,
    args: &[String],
    timeout: Duration,
    provider: &str,
) -> Result<RawOutput, CodexBarError> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // New process group so terminal-generated signals (e.g. Ctrl-C SIGINT)
        // don't reach codexbar; our timeout kill targets the child directly.
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CodexBarError::NotFound {
                bin: bin.display().to_string(),
            });
        }
        Err(e) => return Err(CodexBarError::Spawn(e.to_string())),
    };

    // Drain stdout/stderr on dedicated threads so a child that fills the OS pipe
    // buffer (~64KB) can't deadlock: it would block on write while our `try_wait`
    // loop waits for an exit that never comes. Mirrors seher-ts, which reads the
    // streams concurrently with process completion.
    let stdout_reader = spawn_reader(child.stdout.take());
    let stderr_reader = spawn_reader(child.stderr.take());

    let timeout_ms = timeout.as_millis();
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Readers unblock at EOF once the killed child's pipes close.
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(CodexBarError::Timeout {
                        provider: provider.to_string(),
                        ms: timeout_ms,
                    });
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(CodexBarError::Spawn(e.to_string())),
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    // codexbar exits 4 when its own usage fetch times out internally.
    if status.code() == Some(TIMEOUT_EXIT_CODE) {
        return Err(CodexBarError::Timeout {
            provider: provider.to_string(),
            ms: timeout_ms,
        });
    }

    Ok(RawOutput {
        stdout,
        stderr,
        code: status.code(),
    })
}

fn parse_response(
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
    provider: &str,
) -> Result<CodexBarUsageResponse, CodexBarError> {
    if code != Some(0) {
        return Err(CodexBarError::Exited {
            code,
            provider: provider.to_string(),
            stderr: stderr.trim().to_string(),
        });
    }

    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| CodexBarError::Parse(e.to_string()))?;
    // codexbar emits a JSON array (one entry per provider) even when --provider
    // selects a single one — unwrap to the matching entry.
    let entries = value
        .as_array()
        .ok_or_else(|| CodexBarError::NonArray(provider.to_string()))?;
    for item in entries {
        let Ok(entry) = serde_json::from_value::<CodexBarUsageResponse>(item.clone()) else {
            continue;
        };
        if entry.provider == provider {
            return Ok(entry);
        }
    }
    Err(CodexBarError::NoEntry(provider.to_string()))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    #[test]
    fn resolve_bin_path_prefers_explicit() {
        let p = resolve_bin_path(Some("/custom/codexbar"));
        assert_eq!(p, PathBuf::from("/custom/codexbar"));
    }

    #[test]
    fn parse_response_unwraps_matching_provider() {
        let stdout = r#"[{"provider":"claude","usage":{"primary":{"usedPercent":40}}}]"#;
        let entry = parse_response(stdout, "", Some(0), "claude").expect("entry");
        assert_eq!(entry.provider, "claude");
        let primary = entry.usage.primary.expect("primary");
        assert!((primary.used_percent - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_response_no_entry_for_unknown_provider() {
        let stdout = r#"[{"provider":"claude","usage":{}}]"#;
        let err = parse_response(stdout, "", Some(0), "zai").expect_err("no entry");
        assert!(matches!(err, CodexBarError::NoEntry(_)));
    }

    #[test]
    fn parse_response_nonzero_exit_is_error() {
        let err = parse_response("", "boom", Some(2), "claude").expect_err("exit err");
        assert!(matches!(err, CodexBarError::Exited { .. }));
    }

    #[test]
    fn parse_response_non_array_payload() {
        let err = parse_response("{}", "", Some(0), "claude").expect_err("non-array");
        assert!(matches!(err, CodexBarError::NonArray(_)));
    }
}
