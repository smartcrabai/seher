use super::types::ClaudeTerminalError;

/// Allowed values for `--permission-mode` passed to the `claude` CLI.
const ALLOWED_PERMISSION_MODES: &[&str] = &[
    "bypassPermissions",
    "default",
    "acceptEdits",
    "acceptEditsAndAutoApprove",
];

pub struct BuildClaudeCommandOptions {
    pub claude_bin: String,
    pub permission_mode: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    /// When set, resume the given Claude session id (`claude --resume <id>`) so the
    /// conversation continues in the same transcript instead of starting fresh.
    pub resume_session_id: Option<String>,
}

/// Build the `claude` CLI argument list.
///
/// # Errors
///
/// Returns an error if `permission_mode` is not in the allowed list.
pub fn build_claude_command(
    opts: &BuildClaudeCommandOptions,
) -> Result<Vec<String>, ClaudeTerminalError> {
    if !ALLOWED_PERMISSION_MODES.contains(&opts.permission_mode.as_str()) {
        return Err(ClaudeTerminalError::Other(format!(
            "invalid permission_mode {:?}; allowed: {}",
            opts.permission_mode,
            ALLOWED_PERMISSION_MODES.join(", ")
        )));
    }
    let mut args = vec![opts.claude_bin.clone()];
    if let Some(id) = &opts.resume_session_id {
        args.push("--resume".to_string());
        args.push(id.clone());
    }
    if let Some(m) = &opts.model {
        args.push("--model".to_string());
        args.push(m.clone());
    }
    if let Some(s) = &opts.system_prompt {
        args.push("--append-system-prompt".to_string());
        args.push(s.clone());
    }
    args.push("--permission-mode".to_string());
    args.push(opts.permission_mode.clone());
    Ok(args)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests panic on unexpected fixtures")]
mod tests {
    use super::*;

    fn cmd(opts: &BuildClaudeCommandOptions) -> Vec<String> {
        build_claude_command(opts).expect("valid permission mode")
    }

    #[test]
    fn minimal_no_model_no_system() {
        let args = cmd(&BuildClaudeCommandOptions {
            claude_bin: "claude".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            model: None,
            system_prompt: None,
            resume_session_id: None,
        });
        assert_eq!(args, ["claude", "--permission-mode", "bypassPermissions"]);
    }

    #[test]
    fn with_model() {
        let args = cmd(&BuildClaudeCommandOptions {
            claude_bin: "claude".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            model: Some("claude-opus-4-7".to_string()),
            system_prompt: None,
            resume_session_id: None,
        });
        assert_eq!(
            args,
            [
                "claude",
                "--model",
                "claude-opus-4-7",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
    }

    #[test]
    fn with_system_prompt() {
        let args = cmd(&BuildClaudeCommandOptions {
            claude_bin: "claude".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            model: None,
            system_prompt: Some("Be concise.".to_string()),
            resume_session_id: None,
        });
        assert_eq!(
            args,
            [
                "claude",
                "--append-system-prompt",
                "Be concise.",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
    }

    #[test]
    fn with_model_and_system() {
        let args = cmd(&BuildClaudeCommandOptions {
            claude_bin: "/usr/local/bin/claude".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            system_prompt: Some("sys".to_string()),
            resume_session_id: None,
        });
        assert_eq!(
            args,
            [
                "/usr/local/bin/claude",
                "--model",
                "claude-sonnet-4-6",
                "--append-system-prompt",
                "sys",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
    }

    #[test]
    fn rejects_invalid_permission_mode() {
        let result = build_claude_command(&BuildClaudeCommandOptions {
            claude_bin: "claude".to_string(),
            permission_mode: "dangerousMode".to_string(),
            model: None,
            system_prompt: None,
            resume_session_id: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn with_resume_session_id() {
        let args = cmd(&BuildClaudeCommandOptions {
            claude_bin: "claude".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            model: None,
            system_prompt: None,
            resume_session_id: Some("abc-123".to_string()),
        });
        assert_eq!(
            args,
            [
                "claude",
                "--resume",
                "abc-123",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
    }
}
