# Seher

Seher picks the highest-priority coding agent that is **not** currently rate-limited, then runs a `plan` / `build` prompt through it. If every configured agent is at its limit, seher waits until the earliest reset and tries again.

Prompts are executed in-process by the [`pi`](https://github.com/Dicklesworthstone/pi_agent_rust) agent engine, or via the local `claude` CLI. There are three CLI backends: `claude-terminal` (drives `claude` via tmux), `claude-headless` (runs `claude -p` as a subprocess), and `claude` (drives the CLI through the in-tree [`claude-agent-sdk`](crates/claude-agent-sdk) crate — a Rust port of [`anthropics/claude-agent-sdk-python`](https://github.com/anthropics/claude-agent-sdk-python) — which adds stream-json output, the control protocol, and in-process MCP tools). Rate-limit detection is delegated to the external [`codexbar`](https://codexbar.app/) binary, which seher invokes per provider.

The repository is a Cargo workspace with three crates:

| Crate | Artifact | Purpose |
|-------|----------|---------|
| `crates/seher-cli` | `seher` binary | CLI entry point (argument parsing, plan/build modes, streaming) |
| `crates/seher-sdk` | `seher` library | Agent resolution, codexbar-backed rate-limit checks, pi / claude / claude-terminal / claude-headless runners |
| `crates/claude-agent-sdk` | `claude_agent_sdk` library | Rust port of [`anthropics/claude-agent-sdk-python`](https://github.com/anthropics/claude-agent-sdk-python): drives the `claude` CLI over stream-json with control-protocol support (in-process MCP tools, `query()` / `ClaudeSDKClient`) |


## How it works

1. Seher loads the YAML config and builds a candidate list: every provider that defines a model for the requested mode (`plan` / `build`).
2. Candidates are sorted by **priority** (descending), with ties broken by their order in the config file.
3. Each candidate is probed in order to see whether it is rate-limited. The first non-limited provider wins.
4. If all candidates are limited, seher sleeps until the earliest reset time and rescans.
5. The chosen provider streams the prompt via pi. If pi reports a rate/usage limit mid-run, that provider is excluded and seher re-resolves with the next candidate.

Rate-limit detection is delegated to [`codexbar`](https://codexbar.app/): for each candidate, seher runs `codexbar usage --format json --provider <provider>` and treats the provider as limited when any reported usage window is at 100%. The `claude`, `claude-terminal`, and `claude-headless` SDKs all share the `claude` codexbar account. If codexbar is not installed, returns no entry for a provider, or errors transiently, that provider is treated as available so resolution still proceeds.

codexbar must be installed and on `PATH` (or pointed to via `SEHER_CODEXBAR_BIN`); see [codexbar.app](https://codexbar.app/).


## Installation

### Homebrew (macOS / Linux) — recommended

```sh
brew install smartcrabai/tap/seher
```

### Pre-built binaries

Pre-built binaries are available for macOS and Linux (x86_64 and aarch64):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/smartcrabai/seher/releases/latest/download/seher-installer.sh | sh
```

### Build from source

```sh
cargo install --git https://github.com/smartcrabai/seher seher-cli
```


## Usage

```sh
# Build mode (default) — resolve a build-mode provider and run the prompt
seher "fix bugs"
seher build "fix bugs"

# Plan mode — generate a plan, open it in $EDITOR for review, then execute it
seher plan "add OAuth login"

# No prompt → input via stdin or $EDITOR (defaults to vim)
seher
echo "fix bugs" | seher

# Force a specific provider (matched against the resolved provider name)
seher --provider claude "fix bugs"

# Override the mode/model key used during resolution
seher --model low "fix bugs"
seher -m high plan "design the cache layer"

# Point at a specific config file
seher --config ./my-config.yaml "fix bugs"

# Per-run timeout (milliseconds) and quiet output
seher --timeout 600000 --quiet "fix bugs"

# Show which provider/model/SDK would be selected (dry run)
seher --show-resolution
seher --show-resolution -m plan
seher --show-resolution -p codex

# Multi-turn: a fresh run prints `session: <id>` to stderr; resume it with -r
seher --cwd /path/to/project "implement the feature"   # stderr: session: <uuid>
seher --cwd /path/to/project -r <uuid> "now add tests"
```

### Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--provider <name>` | `-p` | Force a specific provider key (skips all others) |
| `--model <key>` | `-m` | Mode/model key override. Defaults to `plan` in plan mode and `build` in build mode |
| `--config <path>` | `-c` | Path to a YAML config file |
| `--timeout <ms>` | `-t` | Per-run timeout in milliseconds |
| `--quiet` | `-q` | Suppress informational output |
| `--show-resolution` | | Show which provider/model/SDK would be selected and exit (no prompt required). Candidates are listed on stderr; the winner is printed as JSON on stdout |
| `--cwd <dir>` | | Working directory for the agent. Canonicalized on receipt; must exist. Multi-turn sessions are bound to it |
| `--resume <id>` | `-r` | Resume a prior session by id (printed as `session: <id>` on a previous run). Pass the same `--cwd` used to create it |

### Prompt resolution

When no prompt is given on the command line, seher resolves it in this order:

1. Trailing positional arguments, joined with spaces.
2. Standard input, when piped (non-TTY).
3. `$EDITOR` (default `vim`), opened on a temp file.

### Modes

- **`build`** (default): resolves the highest-priority non-limited provider for the `build` mode key and streams the prompt through it.
- **`plan`**: first resolves the `plan` mode key and streams a Markdown implementation plan (the model is instructed to output *only* the plan and touch no files). The plan opens in `$EDITOR` for review/editing; the edited plan is then wrapped and executed under the `build` mode key. Leaving the editor empty cancels the run.

The first trailing token (`plan` or `build`) selects the mode; anything else is treated as the start of the prompt and defaults to build mode. `-m/--model` overrides both the plan and build keys used during resolution.

### Multi-turn sessions

Every run is a persistent session that a follow-up run can continue:

- A fresh run prints `session: <id>` to **stderr** (stdout carries only the assistant text, so piping stays safe).
- Sessions are bound to the working directory. Pass `-r/--resume <id>` together with the **same `--cwd`** used to create the session (`--cwd` is canonicalized up front so symlinked/relative forms of the same directory resolve identically).
- On resume, seher probes the on-disk session storage to find the backend that owns the id and **pins** it: the retry-on-limit provider switch is disabled (a session id is meaningless to a different backend), and a missing session is a hard error. If the resolver would pick a different backend (e.g. the owner is rate-limited), pass `--provider` to force the matching one.

It is convenient to alias frequently used options:

```sh
alias shr="seher --model high --quiet"
```


## Using as a library

The `seher-sdk` crate is published as a library named `seher`. The CLI is a thin
wrapper around it, so anything the binary does is reachable from Rust.

Add it as a git (or path) dependency:

```toml
[dependencies]
seher-sdk = { git = "https://github.com/smartcrabai/seher" }
```

Rate-limit checks shell out to the external `codexbar` binary, so it must be
installed on the host (or pointed to via `SEHER_CODEXBAR_BIN`).

The library exposes two layers.

### Low level — run a prompt through pi

`PiRunner` streams a prompt through the `pi` engine. `stream` returns a channel of
`StreamChunk` values (`Delta`, `Done`, `Session`, `Limit`, `Error`) and runs pi on
its own thread, so it works whether or not the caller hosts a tokio runtime:

```rust
use seher::sdk::{PiRunner, PiRunnerOptions, StreamChunk};

let runner = PiRunner::new(PiRunnerOptions {
    provider: Some("anthropic".to_string()),
    model: Some("claude-sonnet-4-5".to_string()),
    api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
    ..PiRunnerOptions::default()
});

// `None` = fresh session; pass a prior session id to continue a conversation.
let rx = runner.stream("say hi".to_string(), None);
loop {
    match rx.recv() {
        Ok(StreamChunk::Delta(d)) => print!("{d}"),
        Ok(StreamChunk::Session(id)) => eprintln!("session: {id}"),
        Ok(StreamChunk::Done(text)) => {
            println!("{text}");
            break;
        }
        Ok(StreamChunk::Limit(e)) => {
            eprintln!("rate limited: {e}");
            break;
        }
        Ok(StreamChunk::Error(msg)) => {
            eprintln!("error: {msg}");
            break;
        }
        Err(_) => break, // channel closed
    }
}
```

### Working directory and multi-turn sessions

`PiRunnerOptions.working_directory` sets the directory the agent operates in, and
also binds where multi-turn session files live. Both `stream` and the blocking
`run` convenience take a `resume` argument: `None` starts a fresh session (a new
id is generated and emitted as the first chunk via `StreamChunk::Session`), and
`Some(id)` continues a prior turn — pass the same `working_directory` the session
was created with:

```rust
use seher::sdk::{PiRunner, PiRunnerOptions};

let runner = PiRunner::new(PiRunnerOptions {
    provider: Some("anthropic".to_string()),
    model: Some("claude-sonnet-4-5".to_string()),
    api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
    working_directory: Some("/path/to/project".into()),
    ..PiRunnerOptions::default()
});

// Turn 1 — fresh session. `run` returns the full text plus the session id.
let first = runner.run("implement the feature".to_string(), None)?;

// Turn 2 — same runner options, resumed by id.
let second = runner.run("now add tests".to_string(), Some(first.session_id))?;
```

Session files are stored at a deterministic per-`(cwd, id)` path (see
`pi_session_path`), so the same pair always resumes the same conversation. The
`claude`, `claude-terminal`, and `claude-headless` backends support the same
contract via `claude --resume <id>`.

### Custom tools (function calling)

`PiRunnerOptions.tools` injects custom tools into the agent session before the
prompt runs. A `SeherTool` pairs a name/description and a JSON Schema
(`type: object` with `properties`) with a synchronous handler. The handler
receives the raw JSON input the model produced; `Ok(text)` becomes the tool
result, and `Err(message)` is fed back to the model with `is_error: true` —
standard function-calling behavior, so the model can recover or retry without
aborting the turn:

```rust
use std::sync::Arc;
use seher::sdk::{PiRunner, PiRunnerOptions, SeherTool};

let weather = SeherTool::new(
    "get_weather",
    "Get the current weather for a city",
    serde_json::json!({
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"],
    }),
    Arc::new(|input| {
        let city = input["city"]
            .as_str()
            .ok_or_else(|| "missing city".to_string())?;
        Ok(format!("Sunny in {city}"))
    }),
);

let runner = PiRunner::new(PiRunnerOptions {
    provider: Some("anthropic".to_string()),
    model: Some("claude-sonnet-4-5".to_string()),
    api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
    tools: vec![weather],
    ..PiRunnerOptions::default()
});
```

Tool names must be unique and must not collide with pi's built-in tools; an
invalid set fails fast with `StreamChunk::Error` before any session file is
created.

Tool-capable SDKs:

- **`pi`** — tools are injected directly into the in-process agent session.
- **`claude`** — tools are served through the SDK MCP control channel of
  `claude-agent-sdk` (in-process JSON-RPC, no external server needed). Pass
  the same `Vec<SeherTool>` via `ClaudeAgentRunnerConfig.tools` in
  `seher::claude_agent::stream_agent`. The toolbox is auto-registered under
  `--mcp-config` as `{"type": "sdk", "name": "seher"}`; allow the tools by
  name (e.g. `mcp__seher__get_weather`) in `allowed_tools` if you want them
  ungated.
- **`claude-terminal` / `claude-headless`** drive the `claude` CLI externally
  and cannot honor custom tools.

When resolving an agent for a run that passes tools, set `require_tools: true`
on `ResolveOptions`/`PollOptions` so resolution drops non-tool-capable
candidates instead of silently ignoring the tools; if every candidate is
dropped, resolution fails with a `NoMatching` error explaining why.

### High level — resolve a non-limited provider, then run

`resolve_agent` applies the same priority + rate-limit logic as the CLI and returns
the winning `ResolvedAgent`. It is async; pair it with `CodexBarProbe` (which queries
the external codexbar binary), or use the `resolve_agent_with_codexbar` convenience
wrapper:

```rust
use seher::sdk::{CodexBarProbe, ResolveOptions, load_config, resolve_agent};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let config = load_config(None)?; // ~/.config/seher/config.yaml (or $SEHER_CONFIG)
let mut probe = CodexBarProbe;

let resolved = resolve_agent(
    ResolveOptions {
        mode_key: "build".to_string(),
        config: Some(config),
        ..Default::default()
    },
    &mut probe,
)
.await?;

println!("selected {} (pi/{})", resolved.provider, resolved.model_id);
// Feed `resolved.model_id` / `resolved.api` into PiRunnerOptions to execute.
# Ok(())
# }
```

See `crates/seher-sdk/examples/pi_mvp.rs` for a runnable example, and
`crates/seher-cli/src/run_mode.rs` for the full
resolve → stream → retry-on-limit loop.

### Driving the `claude` CLI directly

For the `claude` SDK, seher-sdk re-exports the underlying
[`claude-agent-sdk`](crates/claude-agent-sdk) crate and adds a
[`StreamChunk`](crates/seher-sdk/src/sdk/pi_runner.rs)-compatible bridge so the
same consumer code that drains pi can drain claude:

```rust
use seher::claude_agent::{ClaudeAgentRunnerConfig, stream_agent};
use seher::sdk::{SeherTool, StreamChunk};
use std::sync::Arc;

let weather = SeherTool::new(
    "get_weather",
    "Get the current weather for a city",
    serde_json::json!({
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"],
    }),
    Arc::new(|input| {
        let city = input["city"].as_str().ok_or_else(|| "missing city".to_string())?;
        Ok(format!("Sunny in {city}"))
    }),
);

let rx = stream_agent(
    ClaudeAgentRunnerConfig {
        model: Some("claude-sonnet-4-6".into()),
        tools: vec![weather],
        // The toolbox name is `seher`, so allow this prefix to bypass
        // permission prompts when running with `bypassPermissions` is too
        // broad.
        allowed_tools: vec!["mcp__seher__get_weather".into()],
        ..Default::default()
    },
    "What's the weather in Tokyo?".into(),
    "claude".into(), // provider label, surfaced on rate-limit errors
);
```

For the raw async API (`Stream<Item = Message>`, `ClaudeSDKClient`, custom
`Transport`, etc.) use the crate directly:

```rust
use seher::claude_agent_sdk::{query, ClaudeAgentOptions, Message, PermissionMode};
use futures::StreamExt as _;

let opts = ClaudeAgentOptions {
    permission_mode: Some(PermissionMode::BypassPermissions),
    ..Default::default()
};
let mut stream = query("Summarize the README.", Some(opts), None).await?;
while let Some(msg) = stream.next().await {
    if let Message::Assistant(a) = msg? {
        // …
    }
}
```

Runnable examples: `crates/claude-agent-sdk/examples/quickstart.rs`,
`crates/claude-agent-sdk/examples/with_tools.rs`, and
`crates/seher-sdk/examples/claude_agent_via_seher.rs`.


## Configuration

Seher reads a YAML config resolved in this order:

1. `-c <path>` (command-line override)
2. `$SEHER_CONFIG` environment variable
3. `~/.config/seher/config.yaml`

If none of these exist, an empty config is used (and no providers are available).

### Format

```yaml
# Top-level skill discovery defaults (optional)
skills:
  includeClaude: true

providers:
  # Map key doubles as the provider label and the default provider name.
  claude:
    priority: 100            # provider-level priority shorthand (optional)
    models:
      plan: anthropic/claude-opus-4-5
      build: anthropic/claude-sonnet-4-5

  codex:
    models:
      # A model entry can be a bare string or a full object with its own priority.
      plan: { model: openai/gpt-5.5, priority: 50 }
      build: { model: openai/gpt-5.5, priority: 40 }

  claude-headless:
    sdk: claude-headless     # runs `claude -p` as a subprocess (no tmux needed)
    priority: 50
    models:
      plan: claude-opus-4-7
      build: claude-sonnet-4-6

  claude-sdk:
    sdk: claude              # claude-agent-sdk: stream-json + in-process MCP tools
    priority: 60
    models:
      plan: claude-opus-4-7
      build: claude-sonnet-4-6

  zai:
    provider: zai            # explicit provider name (overrides the map key)
    sdk: pi                  # execution engine; defaults to "pi"
    api:
      key: sk-your-key
      endpoint: https://api.z.ai/...
    skills:
      includeClaude: false   # per-provider override of the top-level default
    models:
      build: zai/glm-4.6
```

### Provider fields

| Field | Type | Description |
|-------|------|-------------|
| *(map key)* | string | Provider label and default provider name |
| `provider` | string | Explicit provider name; defaults to the map key |
| `sdk` | string | Execution engine. Defaults to `"pi"`. Executable engines in this build: `pi` (in-process), `claude` (drives the local `claude` CLI through `claude-agent-sdk` — stream-json + in-process MCP tools), `claude-terminal` (via tmux), and `claude-headless` (runs `claude -p` as a subprocess); other kinds are filtered out (see *Cross-implementation portability*). `pi` and `claude` support custom tools; the two CLI-only backends do not |
| `priority` | integer (`i32`) | Provider-level priority. Used when a model entry omits its own `priority` |
| `api.key` | string | API key (for API-key-based limit checks and pi execution) |
| `api.endpoint` | string | API endpoint override |
| `skills.includeClaude` | boolean | Whether to auto-discover Claude skills for this provider |
| `models` | map | **Required.** Maps a mode key (`plan`, `build`, or any custom key passed via `-m`) to a model |

### Model entries

A `models` value is either a bare model-id string or an object `{ model, priority }`:

```yaml
models:
  build: anthropic/claude-sonnet-4-5          # bare string
  plan: { model: anthropic/claude-opus-4-5, priority: 10 }   # full form
  high: anthropic/claude-opus-4-5:high        # with a thinking level
```

The **model id** uses a `provider/model` shape. The segment before the first `/` is passed to pi as the provider (e.g. `anthropic`, `openai`); the rest is the model name. A model id without a `/` is passed through as the model with no explicit provider.

A trailing `:` suffix on the model name selects pi's **thinking level**: `model:thinking` (e.g. `anthropic/claude-opus-4-5:high`, `opus-4.7:medium`). Recognized levels are `off`, `minimal`, `low`, `medium`, `high`, and `xhigh` (plus the aliases pi accepts: `none`/`0`, `min`, `1`, `med`/`2`, `3`, `4`). A suffix that is not a recognized level stays part of the model name, so OpenRouter-style variants like `openrouter/meta-llama/llama-3.1-8b-instruct:free` keep working. The level only applies to pi execution — with the `claude`, `claude-terminal`, and `claude-headless` SDKs a recognized suffix is stripped and ignored. Without a suffix, pi's default (no extended thinking) is used.

For pi execution, the API key comes from `api.key`, falling back to `ANTHROPIC_API_KEY` (when the model provider is `anthropic`) or `OPENAI_API_KEY` (when it is `openai`).

### Priority and ordering

For each candidate, the effective priority is: the model entry's `priority`, else the provider's `priority`, else `0`. Candidates are sorted by priority descending; ties are broken by the provider's order in the YAML file (earlier wins).

`--provider <name>` restricts resolution to providers whose resolved name matches exactly.


## Providers and rate-limit tracking

Rate-limit checks are delegated to the external `codexbar` binary. For each candidate, seher runs:

```sh
codexbar usage --format json --provider <provider>
```

The provider name passed to codexbar is the resolved provider (the map key, or the explicit `provider` field); the `claude`, `claude-terminal`, and `claude-headless` SDKs are mapped to the `claude` codexbar account. A provider is considered limited when any usage window codexbar reports (primary / secondary / tertiary / extra windows) is at 100%, and seher waits until the earliest reset.

Whichever providers codexbar can report on are limit-checked; any provider codexbar has no entry for (or any codexbar error / missing binary) is treated as always-available, which is also useful for routing a custom backend purely by priority.

### Example: an API-key provider

```yaml
providers:
  openrouter:
    api:
      key: sk-or-v1-your-key-here
    models:
      build: openrouter/anthropic/claude-sonnet-4-5
```


## Cross-implementation portability

Seher has a TypeScript counterpart (`seher-ts`) that supports additional `sdk` engines (`codex`, `copilot`, `cursor`, `kimi`, `opencode`, …). To keep a single `config.yaml` portable between both implementations, this Rust build **accepts** providers tagged with those SDK kinds but silently filters them out of the candidate list (executable engines here are `pi`, `claude`, `claude-terminal`, and `claude-headless`). A one-time warning is printed at startup for each skipped provider.


## License

Apache-2.0. See [LICENSE](LICENSE).
