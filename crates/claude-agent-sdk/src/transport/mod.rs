//! Transport abstraction for talking to a `claude` CLI process (or stub).

mod subprocess_cli;

pub use subprocess_cli::SubprocessCliTransport;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::errors::Result;

/// Bidirectional message transport.
///
/// Mirrors `claude_agent_sdk._internal.transport.Transport`:
///
/// - [`connect`](Transport::connect) spawns / opens the channel.
/// - [`write`](Transport::write) sends a single JSON line (no trailing
///   newline — the impl adds one).
/// - [`take_message_stream`](Transport::take_message_stream) hands out the
///   stream of incoming JSON frames **once**. Subsequent calls return an
///   immediately-errored stream. The returned stream is `'static`, so it can
///   be polled independently of the transport (the transport itself can stay
///   in the caller for `write`/`end_input`/`close`).
/// - [`end_input`](Transport::end_input) signals end-of-stream to the peer
///   (closes stdin for subprocess transports).
/// - [`close`](Transport::close) tears the connection down.
/// - [`is_ready`](Transport::is_ready) reports whether the transport is
///   currently usable.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&mut self) -> Result<()>;
    async fn write(&mut self, line: &str) -> Result<()>;
    fn take_message_stream(&mut self) -> BoxStream<'static, Result<serde_json::Value>>;
    async fn end_input(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
    fn is_ready(&self) -> bool;
}
