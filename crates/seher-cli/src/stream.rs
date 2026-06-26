use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use seher::sdk::{CancelToken, StreamChunk};

/// Result of [`drain_to_stdout`]. `Limit` carries no payload — the caller already
/// has the [`crate::run_mode`] `ResolvedAgent` whose `provider` is what gets
/// added to the exclude list.
pub enum Outcome {
    Done(String),
    Limit,
    Error(String),
    Timeout,
    Cancelled,
}

/// Where the stream drain should emit output.
#[derive(Clone, Copy)]
pub enum StreamOutput {
    /// Write deltas and the final newline to the supplied writer (stdout in production).
    Stdout,
    /// Suppress all output to the writer; only collect the concatenated text.
    CaptureOnly,
}

/// Drain a `Receiver<StreamChunk>` according to `output`, writing each delta as
/// it arrives only when [`StreamOutput::Stdout`] is selected.
///
/// `timeout_ms` is the total deadline (in ms) from "now" — it does NOT abort the
/// in-flight worker thread; on timeout, the receiver is dropped and the worker
/// is left to finish in the background. Returns `Outcome::Done` with the
/// concatenated text on success.
#[expect(
    clippy::needless_pass_by_value,
    reason = "takes ownership of the receiver so it is dropped on return, signaling the worker the consumer is gone"
)]
pub fn drain_stream<W: Write>(
    rx: Receiver<StreamChunk>,
    timeout_ms: Option<u64>,
    cancel: &CancelToken,
    output: StreamOutput,
    writer: &mut W,
) -> Outcome {
    // Short poll interval used when there is no deadline — lets cancel checks
    // fire even while blocked on recv, instead of waiting for the next chunk.
    const CANCEL_POLL: Duration = Duration::from_millis(50);
    let mut full = String::new();
    let deadline = timeout_ms.map(|t| Instant::now() + Duration::from_millis(t));

    loop {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        let chunk = match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return Outcome::Timeout;
                }
                match rx.recv_timeout(d - now) {
                    Ok(c) => c,
                    Err(RecvTimeoutError::Timeout) => return Outcome::Timeout,
                    // The worker is required to send Done/Limit/Error before dropping
                    // the sender; an unexpected disconnect is a worker panic / early-drop bug.
                    Err(RecvTimeoutError::Disconnected) => {
                        return Outcome::Error(
                            "pi worker disconnected without a terminal chunk".to_string(),
                        );
                    }
                }
            }
            // Without a deadline, use a short timeout so that cancel signals
            // are detected promptly rather than waiting for the next chunk.
            None => loop {
                match rx.recv_timeout(CANCEL_POLL) {
                    Ok(c) => break c,
                    Err(RecvTimeoutError::Timeout) => {
                        if cancel.is_cancelled() {
                            return Outcome::Cancelled;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Outcome::Error(
                            "pi worker disconnected without a terminal chunk".to_string(),
                        );
                    }
                }
            },
        };
        match chunk {
            StreamChunk::Delta(d) => {
                if matches!(output, StreamOutput::Stdout) {
                    let _ = writer.write_all(d.as_bytes());
                    let _ = writer.flush();
                }
                full.push_str(&d);
            }
            // Session id is metadata — keep stdout clean for piping and surface it on
            // stderr so a follow-up turn can resume with `--resume <id>`.
            StreamChunk::Session(id) => {
                eprintln!("session: {id}");
            }
            StreamChunk::Done(t) => {
                if matches!(output, StreamOutput::Stdout) {
                    let _ = writer.write_all(b"\n");
                    let _ = writer.flush();
                }
                return Outcome::Done(if t.is_empty() { full } else { t });
            }
            StreamChunk::Limit(_) => return Outcome::Limit,
            StreamChunk::Error(m) => {
                // If cancellation is active, the error was most likely caused
                // by the runner aborting due to the cancel signal — report it
                // as Cancelled rather than a generic error.
                if cancel.is_cancelled() {
                    return Outcome::Cancelled;
                }
                return Outcome::Error(m);
            }
        }
    }
}

/// Drain a `Receiver<StreamChunk>` to stdout, writing each delta as it arrives.
///
/// See [`drain_stream`] for details.
pub fn drain_to_stdout(
    rx: Receiver<StreamChunk>,
    timeout_ms: Option<u64>,
    cancel: &CancelToken,
) -> Outcome {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    drain_stream(rx, timeout_ms, cancel, StreamOutput::Stdout, &mut out)
}

/// Drain a `Receiver<StreamChunk>` without writing to stdout, returning the
/// concatenated text on success.
///
/// See [`drain_stream`] for details.
pub fn drain_to_capture(
    rx: Receiver<StreamChunk>,
    timeout_ms: Option<u64>,
    cancel: &CancelToken,
) -> Outcome {
    drain_stream(
        rx,
        timeout_ms,
        cancel,
        StreamOutput::CaptureOnly,
        &mut std::io::sink(),
    )
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn no_cancel() -> CancelToken {
        CancelToken::new()
    }

    #[test]
    fn done_returns_concatenated_deltas() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("ab".to_string())).unwrap();
        tx.send(StreamChunk::Delta("cd".to_string())).unwrap();
        tx.send(StreamChunk::Done(String::new())).unwrap();
        drop(tx);
        match drain_to_stdout(rx, None, &no_cancel()) {
            Outcome::Done(s) => assert_eq!(s, "abcd"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn done_with_explicit_text_overrides_buffered_deltas() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("ignored".to_string())).unwrap();
        tx.send(StreamChunk::Done("final".to_string())).unwrap();
        drop(tx);
        match drain_to_stdout(rx, None, &no_cancel()) {
            Outcome::Done(s) => assert_eq!(s, "final"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn limit_returns_limit_outcome() {
        use seher::sdk::LimitError;
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("partial".to_string())).unwrap();
        tx.send(StreamChunk::Limit(LimitError {
            provider: "anthropic".to_string(),
            reset_at: None,
        }))
        .unwrap();
        drop(tx);
        match drain_to_stdout(rx, None, &no_cancel()) {
            Outcome::Limit => {}
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn error_chunk_returns_error_outcome() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Error("boom".to_string())).unwrap();
        drop(tx);
        match drain_to_stdout(rx, None, &no_cancel()) {
            Outcome::Error(m) => assert_eq!(m, "boom"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn disconnected_without_terminal_returns_error() {
        // tx is dropped before sending Done/Limit/Error — must NOT be reported as success.
        let (tx, rx) = channel::<StreamChunk>();
        drop(tx);
        match drain_to_stdout(rx, None, &no_cancel()) {
            Outcome::Error(m) => assert!(m.contains("disconnected"), "got: {m}"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn disconnected_with_timeout_set_returns_error() {
        // Same as above but with a timeout configured — the disconnect path
        // through recv_timeout must also classify as Error, not Timeout.
        let (tx, rx) = channel::<StreamChunk>();
        drop(tx);
        match drain_to_stdout(rx, Some(10_000), &no_cancel()) {
            Outcome::Error(m) => assert!(m.contains("disconnected"), "got: {m}"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn timeout_fires_when_no_chunk_arrives() {
        let (tx, rx) = channel::<StreamChunk>();
        // Keep tx alive in scope so the channel doesn't disconnect; otherwise
        // we'd get the Error branch instead of Timeout.
        match drain_to_stdout(rx, Some(50), &no_cancel()) {
            Outcome::Timeout => {}
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
        drop(tx);
    }

    #[test]
    fn cancelled_token_returns_cancelled_outcome_before_any_chunk() {
        // Given: a token that is already cancelled and a channel with no chunks yet
        let (tx, rx) = channel::<StreamChunk>();
        let cancel = CancelToken::new();
        cancel.cancel();
        // When: drain_to_stdout is called with the already-cancelled token
        // Then: returns Outcome::Cancelled without blocking
        match drain_to_stdout(rx, Some(5_000), &cancel) {
            Outcome::Cancelled => {}
            other => panic!(
                "expected Cancelled, got: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
        drop(tx);
    }

    #[test]
    fn cancelled_token_returns_cancelled_even_with_pending_deltas() {
        // Given: a cancelled token and a channel that has deltas queued but no Done
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("partial".to_string())).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        // When: drain_to_stdout is called
        // Then: returns Cancelled (not Done) because the token was cancelled
        match drain_to_stdout(rx, Some(5_000), &cancel) {
            Outcome::Cancelled => {}
            other => panic!(
                "expected Cancelled, got: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
        drop(tx);
    }

    // -----------------------------------------------------------------------
    // Capture-only output policy
    // -----------------------------------------------------------------------

    #[test]
    fn capture_only_returns_concatenated_deltas() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("ab".to_string())).unwrap();
        tx.send(StreamChunk::Delta("cd".to_string())).unwrap();
        tx.send(StreamChunk::Done(String::new())).unwrap();
        drop(tx);
        match drain_to_capture(rx, None, &no_cancel()) {
            Outcome::Done(s) => assert_eq!(s, "abcd"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn capture_only_writes_nothing_to_writer() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("must not appear".to_string()))
            .unwrap();
        tx.send(StreamChunk::Done(String::new())).unwrap();
        drop(tx);
        let mut buf: Vec<u8> = Vec::new();
        let outcome = drain_stream(rx, None, &no_cancel(), StreamOutput::CaptureOnly, &mut buf);
        assert!(matches!(outcome, Outcome::Done(_)), "expected Done");
        assert!(
            buf.is_empty(),
            "CaptureOnly must not write deltas or final newline"
        );
    }

    #[test]
    fn capture_only_stdout_policy_writes_deltas_and_final_newline() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("hello".to_string())).unwrap();
        tx.send(StreamChunk::Done(String::new())).unwrap();
        drop(tx);
        let mut buf: Vec<u8> = Vec::new();
        let outcome = drain_stream(rx, None, &no_cancel(), StreamOutput::Stdout, &mut buf);
        assert!(matches!(outcome, Outcome::Done(_)), "expected Done");
        assert_eq!(String::from_utf8(buf).unwrap(), "hello\n");
    }

    #[test]
    fn capture_only_limit_returns_limit_outcome() {
        use seher::sdk::LimitError;
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("partial".to_string())).unwrap();
        tx.send(StreamChunk::Limit(LimitError {
            provider: "anthropic".to_string(),
            reset_at: None,
        }))
        .unwrap();
        drop(tx);
        match drain_to_capture(rx, None, &no_cancel()) {
            Outcome::Limit => {}
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn capture_only_error_chunk_returns_error_outcome() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Error("boom".to_string())).unwrap();
        drop(tx);
        match drain_to_capture(rx, None, &no_cancel()) {
            Outcome::Error(m) => assert_eq!(m, "boom"),
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
    }

    #[test]
    fn capture_only_timeout_returns_timeout_outcome() {
        let (tx, rx) = channel::<StreamChunk>();
        match drain_to_capture(rx, Some(50), &no_cancel()) {
            Outcome::Timeout => {}
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
        drop(tx);
    }

    #[test]
    fn capture_only_cancelled_token_returns_cancelled_outcome() {
        let (tx, rx) = channel::<StreamChunk>();
        let cancel = CancelToken::new();
        cancel.cancel();
        match drain_to_capture(rx, Some(5_000), &cancel) {
            Outcome::Cancelled => {}
            other => panic!(
                "expected Cancelled, got: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
        drop(tx);
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    struct OutcomeDebug<'a>(&'a Outcome);
    impl std::fmt::Debug for OutcomeDebug<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                Outcome::Done(s) => write!(f, "Done({s:?})"),
                Outcome::Limit => write!(f, "Limit"),
                Outcome::Error(m) => write!(f, "Error({m:?})"),
                Outcome::Timeout => write!(f, "Timeout"),
                Outcome::Cancelled => write!(f, "Cancelled"),
            }
        }
    }
}
