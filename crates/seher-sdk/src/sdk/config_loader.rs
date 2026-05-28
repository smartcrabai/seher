//! Load `seher` YAML config from disk.
//!
//! Resolution order (TS parity):
//!   1. `-c <path>` (caller-supplied)
//!   2. `$SEHER_CONFIG`
//!   3. `~/.config/seher/config.yaml`

use std::path::{Path, PathBuf};

use super::config::{Config, ConfigRaw};

const SUPPORTED_SDK_KINDS: &[&str] = &["pi"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Failed to read config file '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to parse YAML config: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("Invalid config: {0}")]
    Invalid(String),
    #[error("$HOME is not set; cannot resolve default config path")]
    HomeNotSet,
}

/// Resolve the YAML config path using TS parity rules. Returns `None` when no
/// path is supplied and the default `~/.config/seher/config.yaml` does not exist.
pub fn resolve_config_path(override_path: Option<&Path>) -> Result<Option<PathBuf>, ConfigError> {
    if let Some(p) = override_path {
        return Ok(Some(p.to_path_buf()));
    }
    if let Ok(env_path) = std::env::var("SEHER_CONFIG")
        && !env_path.is_empty()
    {
        return Ok(Some(PathBuf::from(env_path)));
    }
    let home = dirs::home_dir().ok_or(ConfigError::HomeNotSet)?;
    let default = home.join(".config").join("seher").join("config.yaml");
    if default.exists() {
        return Ok(Some(default));
    }
    Ok(None)
}

/// Load and normalize the YAML config from the resolved path. If no file is found,
/// returns the default (empty) config — same as TS `loadConfig` returning the
/// default empty config.
///
/// # Errors
///
/// Returns [`ConfigError`] on filesystem or parse failures, or on validation issues
/// (e.g. provider entry without any models).
pub fn load_config(override_path: Option<&Path>) -> Result<Config, ConfigError> {
    let Some(path) = resolve_config_path(override_path)? else {
        return Ok(Config::default());
    };
    let bytes = std::fs::read(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    let raw: ConfigRaw = serde_yaml::from_slice(&bytes)?;
    let cfg: Config = raw.into();
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    for entry in &cfg.providers {
        if !SUPPORTED_SDK_KINDS.contains(&entry.sdk.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "Provider '{}': unknown sdk '{}' (supported: {})",
                entry.key,
                entry.sdk,
                SUPPORTED_SDK_KINDS.join(", "),
            )));
        }
        if entry.models.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "Provider '{}' defines no models",
                entry.key
            )));
        }
        for (mode_key, m) in &entry.models {
            if m.model.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "Provider '{}' model '{}' has empty model id",
                    entry.key, mode_key
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn missing_file_at_override_path_errors() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("does-not-exist.yaml");
        let err = load_config(Some(&path)).expect_err("should fail on missing file");
        assert!(matches!(err, ConfigError::Io { .. }));
        Ok(())
    }

    #[test]
    fn parses_sample_yaml() -> TestResult {
        let yaml = "
providers:
  claude:
    priority: 3
    models:
      plan: opus-4.7
      build: sonnet-4.6
  codex:
    models:
      plan: { model: gpt-5.5, priority: 5 }
      build: { model: gpt-5.5, priority: 4 }
";
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), yaml)?;
        let cfg = load_config(Some(tmp.path()))?;
        assert_eq!(cfg.providers.len(), 2);
        let claude = &cfg.providers[0];
        assert_eq!(claude.key, "claude");
        assert_eq!(claude.priority, Some(3));
        assert_eq!(claude.models["plan"].model, "opus-4.7");
        assert_eq!(claude.models["plan"].priority, None);
        let codex = &cfg.providers[1];
        assert_eq!(codex.models["plan"].priority, Some(5));
        Ok(())
    }

    #[test]
    fn rejects_provider_without_models() -> TestResult {
        let yaml = "
providers:
  bare:
    sdk: pi
    models: {}
";
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), yaml)?;
        let err = load_config(Some(tmp.path())).expect_err("should reject");
        assert!(matches!(err, ConfigError::Invalid(_)));
        Ok(())
    }

    #[test]
    fn rejects_unknown_sdk() -> TestResult {
        let yaml = "
providers:
  bogus:
    sdk: not-real
    models:
      build: x/y
";
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), yaml)?;
        let err = load_config(Some(tmp.path())).expect_err("should reject");
        let msg = format!("{err}");
        assert!(msg.contains("unknown sdk"), "got: {msg}");
        Ok(())
    }
}
