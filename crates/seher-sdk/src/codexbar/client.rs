//! Spawns the external `codexbar` binary and parses its JSON usage payload.
//!
//! Mirrors `seher-ts/packages/sdk/src/codexbar/client.ts`. We follow the
//! codebase convention of running blocking `std::process` work on a Tokio
//! blocking thread (see `kiro::client`) rather than pulling in the tokio
//! `process` feature. A hard timeout is enforced by polling `try_wait` and
//! killing the child once the deadline passes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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

/// Build the `codexbar` invocation with the platform-appropriate isolation.
///
/// On Unix the child is detached into its own session (`setsid`): `CodexBarCLI`
/// probes agent CLIs through an internal pty and, when it shares the caller's
/// session, it moves the controlling terminal's foreground process group to
/// its own group and exits without restoring it. Ctrl-C then signals a dead
/// group and the host process becomes uninterruptible. A fresh session has no
/// controlling terminal, so codexbar cannot touch ours. It also implies a new
/// process group, so terminal-generated signals (e.g. Ctrl-C SIGINT) don't
/// reach codexbar, and our timeout kill can take down that whole group. A mere
/// `process_group(0)` is not enough — the child would stay in our session and
/// could still claim the terminal.
fn build_command(bin: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // SAFETY: the closure calls setsid(2) and, on failure, builds an
        // io::Error from errno; both are async-signal-safe and allocation-free,
        // so it is safe to run between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // setsid fails only when the caller already leads a process
                // group; a freshly forked child never does, but surface the
                // error instead of silently keeping the parent's session.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd
}

/// SIGKILL the child's whole process group, falling back to the child alone.
///
/// After [`build_command`]'s setsid the child leads its own process group
/// (pgid == its pid), so a negative-pid kill(2) also takes down any probe
/// helpers codexbar spawned into that group. The caller still holds the
/// unreaped child handle, so the pid cannot have been recycled.
fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill(2) takes the pgid by value and touches no memory.
        if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
            return;
        }
    }
    let _ = child.kill();
}

fn run_blocking(
    bin: &Path,
    args: &[String],
    timeout: Duration,
    provider: &str,
) -> Result<RawOutput, CodexBarError> {
    let mut cmd = build_command(bin, args);

    let mut child = match cmd.spawn() {
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
                    kill_process_group(&mut child);
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
    // selects a single one -- unwrap to the matching entry.
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

    /// Guards the terminal-safety property of [`build_command`]: the child must
    /// lead a brand-new session, otherwise codexbar can steal the controlling
    /// terminal's foreground process group and make the host process
    /// uninterruptible via Ctrl-C.
    #[cfg(unix)]
    #[test]
    fn build_command_detaches_child_into_own_session() {
        let mut child = build_command(Path::new("/bin/sleep"), &["5".into()])
            .spawn()
            .expect("spawn sleep");
        // `spawn` reports exec errors through the parent, so by the time it
        // returns Ok the pre_exec hook (setsid) has already run.
        let child_pid = i32::try_from(child.id()).expect("pid fits in i32");
        // SAFETY: getsid takes a pid by value and touches no memory.
        let session_of_child = unsafe { libc::getsid(child_pid) };
        // SAFETY: getsid(0) queries the calling process; no memory involved.
        let session_of_parent = unsafe { libc::getsid(0) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            session_of_child, child_pid,
            "child should lead a fresh session"
        );
        assert_ne!(
            session_of_child, session_of_parent,
            "child must not share our session"
        );
    }
}
