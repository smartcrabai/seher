//! MVP smoke test for the `pi_agent_rust` bridge (M1).
//!
//! Usage:
//!   `ANTHROPIC_API_KEY=sk-... cargo run -p seher-sdk --example pi_mvp -- "say hi"`
//!
//! Streams the assistant's text deltas to stdout. Purpose: verify the dedicated-thread
//! pi runtime does not panic when cohabiting a process that could also host tokio.

use std::io::Write;

use seher::sdk::{PiRunner, PiRunnerOptions, StreamChunk};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: pi_mvp <prompt>");
        std::process::exit(1);
    }
    let prompt = args.join(" ");

    let opts = PiRunnerOptions {
        provider: Some("anthropic".to_string()),
        model: Some("claude-sonnet-4-5".to_string()),
        api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        ..PiRunnerOptions::default()
    };
    let runner = PiRunner::new(opts);
    let rx = runner.stream(prompt);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        match rx.recv() {
            Ok(StreamChunk::Delta(d)) => {
                let _ = out.write_all(d.as_bytes());
                let _ = out.flush();
            }
            Ok(StreamChunk::Done(text)) => {
                if !text.is_empty() {
                    let _ = out.write_all(text.as_bytes());
                }
                let _ = out.write_all(b"\n");
                let _ = out.flush();
                return;
            }
            Ok(StreamChunk::Limit(e)) => {
                eprintln!("\nlimit: {e}");
                std::process::exit(1);
            }
            Ok(StreamChunk::Error(msg)) => {
                eprintln!("\nerror: {msg}");
                std::process::exit(1);
            }
            Err(_) => return,
        }
    }
}
