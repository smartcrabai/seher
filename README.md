# Seher

Seher picks the highest-priority coding agent that is **not** currently rate-limited, then runs a `plan` / `build` prompt through it. If every configured agent is at its limit, seher waits until the earliest reset and tries again.

Prompts are executed in-process by the [`pi`](https://github.com/Dicklesworthstone/pi_agent_rust) agent engine. Rate-limit detection is delegated to the external [`codexbar`](https://codexbar.app/) binary, which seher invokes per provider.

The repository is a Cargo workspace with two crates:

| Crate | Artifact | Purpose |
|-------|----------|---------|
| `crates/seher-cli` | `seher` binary | CLI entry point (argument parsing, plan/build modes, streaming) |
| `crates/seher-sdk` | `seher` library | Agent resolution, codexbar-backed rate-limit checks, the pi runner |


## How it works

1. Seher loads the YAML config and builds a candidate list: every provider that defines a model for the requested mode (`plan` / `build`).
2. Candidates are sorted by **priority** (descending), with ties broken by their order in the config file.
3. Each candidate is probed in order to see whether it is rate-limited. The first non-limited provider wins.
4. If all candidates are limited, seher sleeps until the earliest reset time and rescans.
5. The chosen provider streams the prompt via pi. If pi reports a rate/usage limit mid-run, that provider is excluded and seher re-resolves with the next candidate.

Rate-limit detection is delegated to [`codexbar`](https://codexbar.app/): for each candidate, seher runs `codexbar usage --format json --provider <provider>` and treats the provider as limited when any reported usage window is at 100%. The `claude-terminal` SDK shares the `claude` codexbar account. If codexbar is not installed, returns no entry for a provider, or errors transiently, that provider is treated as available so resolution still proceeds.

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
```

### Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--provider <name>` | `-p` | Force a specific provider key (skips all others) |
| `--model <key>` | `-m` | Mode/model key override. Defaults to `plan` in plan mode and `build` in build mode |
| `--config <path>` | `-c` | Path to a YAML config file |
| `--timeout <ms>` | `-t` | Per-run timeout in milliseconds |
| `--quiet` | `-q` | Suppress informational output |

### Prompt resolution

When no prompt is given on the command line, seher resolves it in this order:

1. Trailing positional arguments, joined with spaces.
2. Standard input, when piped (non-TTY).
3. `$EDITOR` (default `vim`), opened on a temp file.

### Modes

- **`build`** (default): resolves the highest-priority non-limited provider for the `build` mode key and streams the prompt through it.
- **`plan`**: first resolves the `plan` mode key and streams a Markdown implementation plan (the model is instructed to output *only* the plan and touch no files). The plan opens in `$EDITOR` for review/editing; the edited plan is then wrapped and executed under the `build` mode key. Leaving the editor empty cancels the run.

The first trailing token (`plan` or `build`) selects the mode; anything else is treated as the start of the prompt and defaults to build mode. `-m/--model` overrides both the plan and build keys used during resolution.

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
`StreamChunk` values (`Delta`, `Done`, `Limit`, `Error`) and runs pi on its own
thread, so it works whether or not the caller hosts a tokio runtime:

```rust
use seher::sdk::{PiRunner, PiRunnerOptions, StreamChunk};

let runner = PiRunner::new(PiRunnerOptions {
    provider: Some("anthropic".to_string()),
    model: Some("claude-sonnet-4-5".to_string()),
    api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
    ..PiRunnerOptions::default()
});

let rx = runner.stream("say hi".to_string());
loop {
    match rx.recv() {
        Ok(StreamChunk::Delta(d)) => print!("{d}"),
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
| `sdk` | string | Execution engine. Defaults to `"pi"`. Only `pi` is executable in this build (see *Cross-implementation portability*) |
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
```

The **model id** uses a `provider/model` shape. The segment before the first `/` is passed to pi as the provider (e.g. `anthropic`, `openai`); the rest is the model name. A model id without a `/` is passed through as the model with no explicit provider.

For pi execution, the API key comes from `api.key`, falling back to `ANTHROPIC_API_KEY` (when the model provider is `anthropic`) or `OPENAI_API_KEY` (when it is `openai`).

### Priority and ordering

For each candidate, the effective priority is: the model entry's `priority`, else the provider's `priority`, else `0`. Candidates are sorted by priority descending; ties are broken by the provider's order in the YAML file (earlier wins).

`--provider <name>` restricts resolution to providers whose resolved name matches exactly.


## Providers and rate-limit tracking

Rate-limit checks are delegated to the external `codexbar` binary. For each candidate, seher runs:

```sh
codexbar usage --format json --provider <provider>
```

The provider name passed to codexbar is the resolved provider (the map key, or the explicit `provider` field); the `claude-terminal` SDK is mapped to the `claude` codexbar account. A provider is considered limited when any usage window codexbar reports (primary / secondary / tertiary / extra windows) is at 100%, and seher waits until the earliest reset.

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

Seher has a TypeScript counterpart (`seher-ts`) that supports additional `sdk` engines (`claude`, `codex`, `copilot`, `cursor`, `kimi`, `opencode`, …). To keep a single `config.yaml` portable between both implementations, this Rust build **accepts** providers tagged with those SDK kinds but silently filters them out of the candidate list (only `sdk: pi` is executable here). A one-time warning is printed at startup for each skipped provider.


## License

Apache-2.0. See [LICENSE](LICENSE).
