use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use seher::sdk::{LimitError, StreamChunk};

pub enum Outcome {
    Done(String),
    Limit(LimitError),
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
            StreamChunk::Limit(e) => return Outcome::Limit(e),
            StreamChunk::Error(m) => return Outcome::Error(m),
        }
    }
}
