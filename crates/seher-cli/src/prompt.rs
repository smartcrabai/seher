use std::io::{IsTerminal, Read, Write};

/// Resolve the prompt text following TS [`resolvePrompt`] semantics:
///   1. trailing positional args → join with space
///   2. stdin (when not a TTY) → use trimmed content
///   3. else, open `$EDITOR` to type
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

    // TTY → editor
    edit_with_seed("")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

/// Open `$EDITOR` (default `vim`) on a temp file seeded with `seed` and return
/// the user-edited content (untrimmed).
///
/// # Errors
///
/// Returns any IO error from creating the temp file or invoking the editor.
pub fn edit_with_seed(seed: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut tmp = tempfile::NamedTempFile::new()?;
    if !seed.is_empty() {
        tmp.write_all(seed.as_bytes())?;
        tmp.flush()?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let status = std::process::Command::new(&editor)
        .arg(tmp.path())
        .status()?;
    if !status.success() {
        return Err(format!("editor '{editor}' exited with status {status}").into());
    }
    Ok(std::fs::read_to_string(tmp.path())?)
}
