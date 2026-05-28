use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OpencodeGoAuthError {
    #[error("could not determine home directory for OpenCode auth.json")]
    HomeDirNotFound,

    #[error("OpenCode auth file not found: {0}")]
    AuthFileNotFound(String),

    #[error("failed to read OpenCode auth file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse OpenCode auth file: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("opencode-go credentials not found in auth.json")]
    MissingProvider,

    #[error("opencode-go auth entry does not contain an API key")]
    MissingKey,
}

#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(rename = "opencode-go")]
    opencode_go: Option<AuthEntry>,
}

#[derive(Debug, Deserialize)]
struct AuthEntry {
    key: Option<String>,
}

pub struct OpencodeGoAuth;

impl OpencodeGoAuth {
    /// # Errors
    ///
    /// Returns an error if the current user's home directory cannot be
    /// resolved.
    pub fn default_path() -> Result<PathBuf, OpencodeGoAuthError> {
        let home = dirs::home_dir().ok_or(OpencodeGoAuthError::HomeDirNotFound)?;
        Ok(home.join(".local/share/opencode/auth.json"))
    }

    /// # Errors
    ///
    /// Returns an error when the auth file is missing, unreadable, malformed,
    /// or does not contain an `opencode-go` API key.
    pub fn read_api_key() -> Result<String, OpencodeGoAuthError> {
        let path = Self::default_path()?;
        Self::read_api_key_from(&path)
    }

    /// # Errors
    ///
    /// Returns an error when the auth file is missing, unreadable, malformed,
    /// or does not contain an `opencode-go` API key.
    pub fn read_api_key_from(path: &Path) -> Result<String, OpencodeGoAuthError> {
        if !path.exists() {
            return Err(OpencodeGoAuthError::AuthFileNotFound(
                path.display().to_string(),
            ));
        }

        let content = std::fs::read_to_string(path)?;
        let auth: AuthFile = serde_json::from_str(&content)?;
        let entry = auth
            .opencode_go
            .ok_or(OpencodeGoAuthError::MissingProvider)?;
        entry.key.ok_or(OpencodeGoAuthError::MissingKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn reads_opencode_go_api_key() -> TestResult {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(
            tmp.path(),
            r#"{
                "opencode-go": {"type": "api", "key": "sk-test"},
                "openai": {"type": "oauth", "access": "tok"}
            }"#,
        )?;

        let key = OpencodeGoAuth::read_api_key_from(tmp.path())?;
        assert_eq!(key, "sk-test");
        Ok(())
    }

    #[test]
    fn rejects_auth_file_without_opencode_go_entry() -> TestResult {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), r#"{"opencode": {"type": "api", "key": "sk"}}"#)?;

        let err = OpencodeGoAuth::read_api_key_from(tmp.path())
            .err()
            .ok_or("expected missing provider error")?;
        assert!(matches!(err, OpencodeGoAuthError::MissingProvider));
        Ok(())
    }

    #[test]
    fn rejects_auth_file_without_key() -> TestResult {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), r#"{"opencode-go": {"type": "api"}}"#)?;

        let err = OpencodeGoAuth::read_api_key_from(tmp.path())
            .err()
            .ok_or("expected missing key error")?;
        assert!(matches!(err, OpencodeGoAuthError::MissingKey));
        Ok(())
    }
}
