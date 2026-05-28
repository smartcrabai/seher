use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use seher::sdk::StreamChunk;

/// Result of [`drain_to_stdout`]. `Limit` carries no payload — the caller already
/// has the [`crate::run_mode`] `ResolvedAgent` whose `provider` is what gets
/// added to the exclude list.
pub enum Outcome {
    Done(String),
    Limit,
    Error(String),
    Timeout,
}

/// Drain a `Receiver<StreamChunk>` to stdout, writing each delta as it arrives.
///
/// `timeout_ms` is the total deadline (in ms) from "now" — it does NOT abort the
/// in-flight worker thread; on timeout, the receiver is dropped and the worker
/// is left to finish in the background. Returns `Outcome::Done` with the
/// concatenated text on success.
pub fn drain_to_stdout(rx: Receiver<StreamChunk>, timeout_ms: Option<u64>) -> Outcome {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut full = String::new();
    let deadline = timeout_ms.map(|t| Instant::now() + Duration::from_millis(t));

    loop {
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
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => {
                    return Outcome::Error(
                        "pi worker disconnected without a terminal chunk".to_string(),
                    );
                }
            },
        };
        match chunk {
            StreamChunk::Delta(d) => {
                let _ = out.write_all(d.as_bytes());
                let _ = out.flush();
                full.push_str(&d);
            }
            StreamChunk::Done(t) => {
                let _ = out.write_all(b"\n");
                let _ = out.flush();
                return Outcome::Done(if t.is_empty() { full } else { t });
            }
            StreamChunk::Limit(_) => return Outcome::Limit,
            StreamChunk::Error(m) => return Outcome::Error(m),
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests may panic on unexpected fixtures"
)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn done_returns_concatenated_deltas() {
        let (tx, rx) = channel();
        tx.send(StreamChunk::Delta("ab".to_string())).unwrap();
        tx.send(StreamChunk::Delta("cd".to_string())).unwrap();
        tx.send(StreamChunk::Done(String::new())).unwrap();
        drop(tx);
        match drain_to_stdout(rx, None) {
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
        match drain_to_stdout(rx, None) {
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
        match drain_to_stdout(rx, None) {
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
        match drain_to_stdout(rx, None) {
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
        match drain_to_stdout(rx, None) {
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
        match drain_to_stdout(rx, Some(10_000)) {
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
        match drain_to_stdout(rx, Some(50)) {
            Outcome::Timeout => {}
            other => panic!(
                "unexpected outcome: {other:?}",
                other = OutcomeDebug(&other)
            ),
        }
        drop(tx);
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    struct OutcomeDebug<'a>(&'a Outcome);
    impl<'a> std::fmt::Debug for OutcomeDebug<'a> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                Outcome::Done(s) => write!(f, "Done({s:?})"),
                Outcome::Limit => write!(f, "Limit"),
                Outcome::Error(m) => write!(f, "Error({m:?})"),
                Outcome::Timeout => write!(f, "Timeout"),
            }
        }
    }
}
