pub(crate) mod command;
pub(crate) mod detect;
pub(crate) mod normalizer;
pub(crate) mod sdk;
pub(crate) mod tmux_backend;
pub(crate) mod transcript;
pub(crate) mod types;

pub use sdk::{
    ClaudeTerminalSdk, ClaudeTerminalSdkConfig, encode_transcript_path, new_sdk_with_defaults,
    stream_via_thread,
};
pub use transcript::{FileSystemTranscriptReader, default_transcript_root, encode_project_dir};
pub use types::{
    ClaudeRunOutput, ClaudeSessionRef, ClaudeTerminalError, ClaudeTerminalResponse,
    ClaudeTranscriptReader, TerminalBackend, TerminalSession,
};
