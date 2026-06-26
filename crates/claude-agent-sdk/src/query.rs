//! Fire-and-forget [`query`] entrypoint.

use futures::stream::{BoxStream, StreamExt};

use crate::errors::Result;
use crate::internal::client::send_user_text;
use crate::transport::{SubprocessCliTransport, Transport};
use crate::types::{ClaudeAgentOptions, Message};

/// Run a one-shot query against the `claude` CLI and stream messages back.
///
/// Equivalent to the Python `claude_agent_sdk.query()`.
///
/// - If `transport` is `None`, a [`SubprocessCliTransport`] in **string mode**
///   is built from `options` and `prompt` (the CLI runs once with `--print`
///   and exits). `options` is fully honored here.
/// - If `transport` is `Some(_)`, the caller is responsible for choosing the
///   mode and pre-applying any configuration. The provided transport is
///   connected if not already, the prompt is pushed as a `user` frame in
///   streaming mode, and stdin is closed so the CLI exits when done.
///   **`options` is *not* applied to a caller-supplied transport** -- fields
///   like `sdk_mcp_server`, `model`, and `allowed_tools` only take effect on
///   the default transport built from `options`. If you need both a custom
///   transport and option-driven configuration, build the transport via
///   [`SubprocessCliTransport::streaming`] / [`SubprocessCliTransport::one_shot`]
///   yourself.
///
/// # Errors
///
/// Returns whatever the underlying transport surfaces -- spawn failures,
/// JSON decode errors, or CLI process errors. Individual yielded items are
/// `Result<Message>` so per-frame parse failures don't kill the whole stream.
pub async fn query(
    prompt: impl Into<String>,
    options: Option<ClaudeAgentOptions>,
    transport: Option<Box<dyn Transport>>,
) -> Result<BoxStream<'static, Result<Message>>> {
    let prompt = prompt.into();

    let mut transport: Box<dyn Transport> = if let Some(mut t) = transport {
        // Caller-supplied transport: `options` is intentionally ignored.
        if !t.is_ready() {
            t.connect().await?;
        }
        send_user_text(t.as_mut(), &prompt, "default").await?;
        t.end_input().await?;
        t
    } else {
        let opts = options.unwrap_or_default();
        let mut t: Box<dyn Transport> = Box::new(SubprocessCliTransport::one_shot(opts, prompt));
        t.connect().await?;
        t
    };

    let raw = transport.take_message_stream();
    let stream = TransportStream {
        _t: transport,
        inner: raw.map(|item| item.and_then(Message::from_frame)).boxed(),
    };
    Ok(Box::pin(stream))
}

/// Newtype that ties the lifetime of the message stream to the owning
/// transport: the transport is dropped (and its child killed) when the caller
/// drops the stream.
struct TransportStream {
    _t: Box<dyn Transport>,
    inner: BoxStream<'static, Result<Message>>,
}

impl futures::Stream for TransportStream {
    type Item = Result<Message>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}
