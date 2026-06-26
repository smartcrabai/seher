use std::io::{IsTerminal, Read, Write};

/// Resolve the prompt text following TS [`resolvePrompt`] semantics:
///   1. trailing positional args -> join with space
///   2. stdin (when not a TTY) -> use trimmed content
///   3. else, open `$EDITOR` to type
///
/// Returns `None` if the resolved prompt is empty.
pub fn resolve(trailing: &[String]) -> Option<String> {
    if !trailing.is_empty() {
        let joined = trailing.join(" ");
        let trimmed = joined.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut content = String::new();
        if stdin.lock().read_to_string(&mut content).is_err() {
            return None;
        }
        let trimmed = content.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    // TTY -> editor
    edit_with_seed("")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

/// Check whether the editor can be safely launched in the current environment.
///
/// On Unix this verifies that the process group owns the controlling terminal
/// (`tcgetpgrp` == `getpgrp`). On non-Unix it checks that stdin/stdout are
/// terminals. Returns an actionable error so callers can fail explicitly
/// instead of being suspended with `SIGTTOU`/`SIGTTIN`.
pub fn ensure_editor_available() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| format!("cannot open controlling terminal /dev/tty: {e}"))?;
        let fd = tty.as_raw_fd();
        // SAFETY: `fd` is valid for the lifetime of `tty`.
        let tty_pgrp = unsafe { libc::tcgetpgrp(fd) };
        if tty_pgrp < 0 {
            return Err("cannot determine foreground process group".into());
        }
        let our_pgrp = unsafe { libc::getpgrp() };
        if tty_pgrp != our_pgrp {
            return Err("seher is not running in the foreground terminal. \
                 Run `fg` to bring it to the foreground, then try again."
                .into());
        }
    }
    #[cfg(not(unix))]
    {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(
                "stdin/stdout is not a terminal; open an interactive terminal to edit.".into(),
            );
        }
    }
    Ok(())
}

/// Open `$EDITOR` (default `vim`) on a temp file seeded with `seed` and return
/// the user-edited content (untrimmed).
///
/// # Errors
///
/// Returns any IO error from creating the temp file or invoking the editor,
/// or [`ensure_editor_available`] if the environment is not TTY-safe.
pub fn edit_with_seed(seed: &str) -> Result<String, Box<dyn std::error::Error>> {
    ensure_editor_available()?;

    let mut tmp = tempfile::NamedTempFile::new()?;
    if !seed.is_empty() {
        tmp.write_all(seed.as_bytes())?;
        tmp.flush()?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let mut cmd = std::process::Command::new(&editor);
    cmd.arg(tmp.path());

    #[cfg(unix)]
    {
        use std::process::Stdio;
        // Re-open /dev/tty explicitly so the editor always talks to the user's
        // terminal even when stdin/stdout have been redirected.
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| format!("cannot open /dev/tty for editor: {e}"))?;
        let stdin = tty.try_clone()?;
        let stdout = tty.try_clone()?;
        let stderr = tty;
        cmd.stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
    }

    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("editor '{editor}' exited with status {status}").into());
    }
    Ok(std::fs::read_to_string(tmp.path())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_words_are_joined_with_spaces() {
        let r = resolve(&["hello".to_string(), "world".to_string()]);
        assert_eq!(r.as_deref(), Some("hello world"));
    }

    #[test]
    fn trailing_single_word_returns_that_word() {
        let r = resolve(&["alone".to_string()]);
        assert_eq!(r.as_deref(), Some("alone"));
    }

    #[test]
    fn trailing_only_whitespace_returns_none() {
        let r = resolve(&["   ".to_string(), "\t".to_string()]);
        assert_eq!(r, None);
    }

    #[test]
    fn trailing_surrounding_whitespace_is_trimmed() {
        let r = resolve(&["  hi  ".to_string()]);
        assert_eq!(r.as_deref(), Some("hi"));
    }

    #[test]
    fn ensure_editor_available_is_callable_without_panic() {
        // Foreground status is environment-dependent; we only verify the
        // function returns a Result (Ok or Err) rather than panicking.
        let _ = ensure_editor_available();
    }

    /// Restores an environment variable to its previous value when dropped,
    /// even if the test panics.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn edit_with_seed_returns_error_when_editor_exits_nonzero() {
        // EDITOR=false always exits with a non-zero status, so edit_with_seed
        // must return an error regardless of the test environment.
        let _guard = EnvVarGuard::set("EDITOR", "false");
        let result = edit_with_seed("seed");
        assert!(
            result.is_err(),
            "edit_with_seed must fail when the editor exits with a non-zero status"
        );
    }
}
