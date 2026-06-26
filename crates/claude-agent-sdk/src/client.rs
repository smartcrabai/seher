//! Stateful agent client.

use futures::stream::{BoxStream, StreamExt};

use crate::errors::{ClaudeSDKError, Result};
use crate::internal::client::send_user_text;
use crate::transport::{SubprocessCliTransport, Transport};
use crate::types::{ClaudeAgentOptions, Message};

const DEFAULT_SESSION_ID: &str = "default";

/// Stateful counterpart to [`query`](crate::query).
///
/// Wraps a [`Transport`] in **streaming mode** so callers can push multiple
/// user prompts and observe responses (and tool activity) frame-by-frame.
///
/// ```no_run
/// use claude_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient};
/// use futures::StreamExt;
///
/// # async fn run() -> claude_agent_sdk::Result<()> {
/// let mut client = ClaudeSDKClient::new(ClaudeAgentOptions::default());
/// client.connect().await?;
/// client.query("Summarize the README.").await?;
/// let mut stream = client.receive_messages();
/// while let Some(msg) = stream.next().await {
///     println!("{:?}", msg?);
/// }
/// drop(stream);
/// client.disconnect().await?;
/// # Ok(()) }
/// ```
pub struct ClaudeSDKClient {
    options: ClaudeAgentOptions,
    transport: Option<Box<dyn Transport>>,
    message_stream: Option<BoxStream<'static, Result<serde_json::Value>>>,
}

impl ClaudeSDKClient {
    #[must_use]
    pub fn new(options: ClaudeAgentOptions) -> Self {
        Self {
            options,
            transport: None,
            message_stream: None,
        }
    }

    /// Inject a custom transport (e.g. for tests). Must not be connected yet --
    /// [`Self::connect`] will connect it.
    #[must_use]
    pub fn with_transport(mut self, transport: Box<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Spawn the CLI (or open the supplied transport) and prepare for queries.
    ///
    /// # Errors
    ///
    /// Returns the transport's connect error (most often
    /// [`ClaudeSDKError::CliNotFound`] if `claude` isn't on `$PATH`).
    pub async fn connect(&mut self) -> Result<()> {
        let opts = self.options.clone();
        let mut transport: Box<dyn Transport> = match self.transport.take() {
            Some(t) => t,
            None => Box::new(SubprocessCliTransport::streaming(opts)),
        };
        if !transport.is_ready() {
            transport.connect().await?;
        }
        // Take the message stream now so subsequent `receive_messages` calls
        // can be issued without re-borrowing the transport.
        self.message_stream = Some(transport.take_message_stream());
        self.transport = Some(transport);
        Ok(())
    }

    /// Send a user message as a `user` frame.
    ///
    /// # Errors
    ///
    /// Returns [`ClaudeSDKError::Connection`] if [`Self::connect`] wasn't
    /// called, or the transport's write error.
    pub async fn query(&mut self, text: impl AsRef<str>) -> Result<()> {
        let t = self.transport_mut()?;
        send_user_text(t, text.as_ref(), DEFAULT_SESSION_ID).await
    }

    /// Stream of incoming messages. Returns an empty/errored stream if the
    /// client isn't connected yet. The stream is owned (`'static`) so it can
    /// be moved or polled independently of the client; you can call
    /// `query()` / `end_input()` while iterating, as long as you don't poll
    /// the stream from another task that also holds `&mut self`.
    pub fn receive_messages(&mut self) -> BoxStream<'static, Result<Message>> {
        match self.message_stream.take() {
            Some(raw) => raw.map(|item| item.and_then(Message::from_frame)).boxed(),
            None => futures::stream::once(async {
                Err(ClaudeSDKError::connection(
                    "ClaudeSDKClient::receive_messages called before connect or after disconnect",
                ))
            })
            .boxed(),
        }
    }

    /// Close stdin so the CLI knows no more prompts are coming. Use this when
    /// you want the CLI to finish its current turn and exit cleanly.
    ///
    /// # Errors
    ///
    /// Returns [`ClaudeSDKError::Connection`] if the client isn't connected.
    pub async fn end_input(&mut self) -> Result<()> {
        let t = self.transport_mut()?;
        t.end_input().await
    }

    /// Tear down the transport. Subsequent calls are no-ops.
    ///
    /// # Errors
    ///
    /// Returns the transport's close error, if any.
    pub async fn disconnect(&mut self) -> Result<()> {
        self.message_stream = None;
        if let Some(mut t) = self.transport.take() {
            t.close().await?;
        }
        Ok(())
    }

    fn transport_mut(&mut self) -> Result<&mut (dyn Transport + '_)> {
        match self.transport.as_deref_mut() {
            Some(t) => Ok(t),
            None => Err(ClaudeSDKError::connection("client not connected")),
        }
    }
}
