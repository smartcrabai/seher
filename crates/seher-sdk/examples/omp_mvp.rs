//! MVP smoke test for the oh-my-pi RPC backend.
//!
//! Usage:
//!   `ANTHROPIC_API_KEY=sk-... cargo run -p seher-sdk --example omp_mvp -- "say hi"`
//!   `cargo run -p seher-sdk --example omp_mvp -- --resume <session-id> "continue"`
//!
//! Streams assistant text deltas to stdout and prints the session id to
//! stderr. The registered `echo` tool exercises omp's native host-tool path.

use std::io::Write;
use std::sync::Arc;

use seher::sdk::{OmpRpcRunner, OmpRpcRunnerOptions, SeherTool, StreamChunk};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let resume = if args.first().is_some_and(|arg| arg == "--resume") {
        if args.len() < 2 {
            eprintln!("usage: omp_mvp [--resume <session-id>] <prompt>");
            std::process::exit(1);
        }
        let id = args.remove(1);
        args.remove(0);
        Some(id)
    } else {
        None
    };
    if args.is_empty() {
        eprintln!("usage: omp_mvp [--resume <session-id>] <prompt>");
        std::process::exit(1);
    }
    let prompt = args.join(" ");

    let echo = SeherTool::new(
        "echo",
        "Return the message unchanged.",
        serde_json::json!({
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"]
        }),
        Arc::new(|input| {
            let message = input
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            eprintln!("echo tool called: {message}");
            Ok(message)
        }),
    );
    let provider = std::env::var("OMP_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
    let model = std::env::var("OMP_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".to_string());
    let api_key = std::env::var("OMP_API_KEY")
        .ok()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
    let runner = OmpRpcRunner::new(OmpRpcRunnerOptions {
        provider: Some(provider),
        model: Some(model),
        api_key,
        tools: vec![echo],
        ..OmpRpcRunnerOptions::default()
    });
    let rx = runner.stream(prompt, resume);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        match rx.recv() {
            Ok(StreamChunk::Delta(delta)) => {
                let _ = out.write_all(delta.as_bytes());
                let _ = out.flush();
            }
            Ok(StreamChunk::Session(id)) => {
                eprintln!("session: {id}");
            }
            Ok(StreamChunk::Done(text)) => {
                if !text.is_empty() {
                    let _ = out.write_all(text.as_bytes());
                }
                let _ = out.write_all(b"\n");
                let _ = out.flush();
                return;
            }
            Ok(StreamChunk::Limit(error)) => {
                eprintln!("\nlimit: {error}");
                std::process::exit(1);
            }
            Ok(StreamChunk::Error(message)) => {
                eprintln!("\nerror: {message}");
                std::process::exit(1);
            }
            Err(_) => return,
        }
    }
}
