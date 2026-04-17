# Seher


Seher is a CLI tool that waits for an agent's Rate Limit to reset, then executes a specified prompt using Claude Code, Codex, OpenRouter, GLM (Zhipu AI), or other compatible CLIs.


## How it works


By default, it retrieves Chrome's cookie information and uses it to call Claude's API to get the Rate Limit reset time. The browser and profile from which cookies are retrieved can be changed with options.

For OpenRouter, seher authenticates with a Management API Key (no browser cookies required) and tracks the credit balance via the OpenRouter Management API. When `total_usage >= total_credits`, the agent is considered rate-limited.

For GLM (Zhipu AI), seher authenticates with a `glm_api_key` (no browser cookies required) and tracks quota usage via the Zhipu AI quota monitoring API. The agent is considered rate-limited when any quota limit reaches 100% utilization.


## Supported Browsers

Seher supports reading cookies from the following browsers:

### Chromium-based browsers
- Chrome
- Microsoft Edge
- Brave
- Chromium
- Vivaldi
- Comet (Perplexity AI browser)
- Dia (The Browser Company)
- ChatGPT Atlas (OpenAI browser)

### Other browsers
- Firefox (all platforms)
- Safari (macOS only - uses sandboxed cookies location)

All Chromium-based browsers use the same cookie storage format and encryption. Firefox uses a different SQLite schema without encryption. Safari uses a proprietary binary format on macOS.

**Note:** On recent versions of macOS, Safari cookies are stored in a sandboxed location: `~/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies`


## Installation

### Homebrew (macOS / Linux) - recommended

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
cargo install --git https://github.com/smartcrabai/seher
```


## Usage


```sh
# Default
seher "fix bugs"
# Launch vim to input a prompt
seher
# Change the browser and profile from which cookies are retrieved
seher --browser edge --profile "Profile 1" "fix bugs"
# Use Firefox
seher --browser firefox --profile "default-release" "fix bugs"
# Use Safari (macOS only)
seher --browser safari "fix bugs"
# Most Claude Code options can be used as is
seher --chrome --disallowedTools "Bash(git:*)" --permission-mode bypassPermissions "fix bugs"
# Use model level (resolved via agent's models map)
seher --model high "fix bugs"
seher -m low "fix bugs"
```


It is recommended to alias frequently used options as follows:


```sh
alias shr="seher --profile 'Profile 1' --permission-mode bypassPermissions"
```


## Configuration


You can customize seher's behavior by creating `~/.config/seher/settings.json` or `~/.config/seher/settings.jsonc`. If both files exist, seher loads `settings.jsonc` first. The loader accepts `//` and `/* */` comments plus trailing commas in either file, but `settings.jsonc` is the recommended filename when you rely on those JSONC features. If neither file exists, the default configuration (using `claude` with no extra arguments) is applied.


### Settings


| Field | Type | Description |
|-------|------|-------------|
| `priority` | array | Priority rules used to choose among non-limited agents |
| `priority[].command` | string | Executable name to match (e.g. `"claude"`, `"codex"`, `"opencode"`) |
| `priority[].provider` | string or null | Provider to match; omitted infers from `command`, `null` matches fallback agents |
| `priority[].model` | string or null | Model key to match; omitted or `null` matches runs without `--model` |
| `priority[].priority` | integer | Priority value (`i32`); higher wins, unmatched combinations default to `0` |
| `priority[].weekdays` | array of strings or null | Weekday ranges to match in `"start-end"` format (0=Sun, 1=Mon, …, 6=Sat, inclusive). e.g. `["1-5"]` for Mon–Fri. Omit to match any day. |
| `priority[].hours` | array of strings or null | Hour ranges to match in `"start-end"` format, half-open `[start, end)`, 0–48. e.g. `["21-27"]` for 21:00–03:00 overnight. Omit to match any hour. |
| `agents` | array | List of agents to use (required) |
| `agents[].command` | string | Executable name (e.g. `"claude"`, `"codex"`, `"opencode"`) |
| `agents[].args` | array of strings | Additional arguments (optional; defaults to `[]`) |
| `agents[].models` | object or null | Model level mapping (optional) |
| `agents[].arg_maps` | object | Exact-match mapping from trailing CLI tokens to replacement token arrays (optional; defaults to `{}`) |
| `agents[].env` | object or null | Environment variables to set when running the agent (optional) |
| `agents[].provider` | string or null | Rate limit provider override (optional, see below) |
| `agents[].openrouter_management_key` | string | Management API key for OpenRouter (required when `provider` is `"openrouter"`) |
| `agents[].glm_api_key` | string | API key for GLM (Zhipu AI) provider (required when `provider` is `"glm"`) |
| `agents[].active` | object or null | Schedule during which the agent is **only** active; disabled outside the window (optional) |
| `agents[].inactive` | object or null | Schedule during which the agent is **disabled**; active outside the window (optional) |
| `agents[].active.weekdays` / `agents[].inactive.weekdays` | array of strings or null | Weekday ranges in `"start-end"` format (0=Sun, 1=Mon, …, 6=Sat, inclusive). e.g. `["1-5"]` for Mon–Fri |
| `agents[].active.hours` / `agents[].inactive.hours` | array of strings or null | Hour ranges in `"start-end"` format, half-open `[start, end)`, 0–48. e.g. `["21-27"]` for 21:00–03:00 overnight |


### JSON Schema


The repository ships a schema at `schemas/settings.schema.json`. To enable editor validation and completion for your local config, add `$schema` like this:


```json
{
  "$schema": "https://raw.githubusercontent.com/smartcrabai/seher/main/schemas/settings.schema.json",
  "agents": [
    {
      "command": "claude"
    }
  ]
}
```

The checked-in sample at `examples/settings.json` also references this schema.


### Example


```json
{
  "$schema": "https://raw.githubusercontent.com/smartcrabai/seher/main/schemas/settings.schema.json",
  "priority": [
    {
      "command": "opencode",
      "provider": "copilot",
      "model": "high",
      "priority": 100
    },
    {
      "command": "codex",
      "priority": 50
    },
    {
      "command": "claude",
      "provider": null,
      "model": "medium",
      "priority": 25
    }
  ],
  "agents": [
    {
      "command": "claude",
      "args": ["--model", "{model}"],
      "models": {
        "high": "opus",
        "medium": "sonnet",
        "low": "haiku",
        "sonnet": "sonnet"
      },
      "arg_maps": {
        "--danger": ["--permission-mode", "bypassPermissions"]
      },
      "env": {
        "CLAUDE_CODE_MAX_TURNS": "100"
      }
    },
    {
      "command": "opencode",
      "provider": "copilot",
      "args": ["--model", "{model}", "--yolo"],
      "models": {
        "high": "github-copilot/gpt-5.4",
        "medium": "github-copilot/gpt-5.4",
        "low": "github-copilot/claude-haiku-4.5"
      }
    },
    {
      "command": "claude",
      "provider": null,
      "args": ["--model", "{model}"],
      "models": {
        "medium": "MiniMax-M2.5",
        "low": "MiniMax-M2.5"
      },
      "env": {
        "ANTHROPIC_AUTH_TOKEN": "your-api-key-here",
        "ANTHROPIC_BASE_URL": "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic"
      }
    }
  ]
}
```

The `{model}` placeholder in `args` is resolved based on the value passed to `--model`. If the key exists in the `models` map, it is replaced with the mapped value; otherwise the value is used as-is. When `--model` is not specified, any argument containing `{model}` is skipped.

`arg_maps` rewrites each trailing CLI token independently using exact-match keys. A mapping value can expand one input token into multiple output tokens, while unmapped tokens are passed through unchanged. For example, with the sample configuration, `seher --danger "fix bugs"` adds `--permission-mode bypassPermissions` when Claude is selected.

`priority` matches the combination of `command`, resolved `provider`, and `--model` key. If a rule's `provider` is omitted, it is inferred from `command` using the same logic as agents (`claude` → `claude`, `codex` → `codex`, `copilot` → `copilot`). Setting `provider` to `null` matches fallback agents. When multiple agents are not rate-limited, seher selects the one with the highest `priority`; if priorities are equal, the earlier entry in `agents` wins.

A rule may also carry optional `weekdays` and `hours` schedule constraints. `weekdays` is a list of inclusive `"start-end"` ranges (0=Sun … 6=Sat); `hours` is a list of half-open `"start-end"` ranges in the 0–48 space (values ≥ 24 wrap to the next calendar day, enabling overnight windows such as `"21-27"` for 21:00–03:00). When multiple rules match the same agent at a given moment, the one with the most schedule constraints (`weekdays` + `hours` axes) wins; ties fall back to the first matching rule.

```jsonc
{
  "priority": [
    // Default daytime priority
    { "command": "claude", "priority": 100 },
    // Boost codex on weekday nights (21:00–03:00)
    { "command": "codex", "priority": 200, "weekdays": ["1-5"], "hours": ["21-27"] }
  ]
}
```

The `provider` field controls rate limit tracking. If omitted, the provider is inferred from the command name (`claude` → claude.ai, `codex` → chatgpt.com, `copilot` → github.com). Setting it to `null` disables rate limit checking for that agent. Setting it to a string (e.g. `"codex"`, `"copilot"`, `"openrouter"`, `"glm"`, or `"opencode-go"`) uses that provider's rate limit regardless of the command name.

For Codex, seher reads `chatgpt.com` browser cookies, fetches an access token from `https://chatgpt.com/api/auth/session`, and then calls `https://chatgpt.com/backend-api/wham/usage`. The request intentionally keeps headers minimal and does not require hard-coding a bearer token in your config.

For OpenRouter, seher does not read browser cookies. Instead, it uses the `openrouter_management_key` value to authenticate with the OpenRouter Management API and check credit balance. The `openrouter_management_key` field is required when `provider` is `"openrouter"`.

```json
{
  "agents": [
    {
      "command": "myai",
      "provider": "openrouter",
      "openrouter_management_key": "sk-or-v1-your-key-here"
    }
  ]
}
```

For GLM (Zhipu AI), seher uses the `glm_api_key` to authenticate with the Zhipu AI quota API. No browser cookies are required. The `glm_api_key` field is required when `provider` is `"glm"`.

```json
{
  "agents": [
    {
      "command": "claude",
      "provider": "glm",
      "glm_api_key": "your-glm-api-key-here",
      "args": ["--model", "{model}"],
      "models": {
        "high": "glm-4-plus"
      }
    }
  ]
}
```

For OpenCode Go, seher reads the local OpenCode history database at `~/.local/share/opencode/opencode.db` and tracks the spend recorded for assistant messages whose `providerID` is `"opencode-go"`. It reports rolling 5-hour, 7-day, and 30-day windows against the documented Go caps (`$12`, `$30`, `$60`). This is local-device tracking, so it reflects usage recorded by your local OpenCode installation rather than the hosted console.

```json
{
  "agents": [
    {
      "command": "opencode",
      "provider": "opencode-go",
      "args": ["--model", "{model}"],
      "models": {
        "medium": "opencode-go/minimax-m2.7"
      }
    }
  ]
}
```

The `env` field specifies environment variables to inject when launching the agent. This is useful for switching API keys or base URLs to route a standard command (e.g. `claude`) to a different backend.

### Per-agent schedules (`active` / `inactive`)

Individual agents can be enabled or disabled based on time-of-day and day-of-week rules:

- `active`: when set, the agent is **only** selectable during the specified schedule; it is disabled outside of it.
- `inactive`: when set, the agent is disabled during the specified schedule; it is active at all other times.
- Setting both `active` and `inactive` on the same agent is rejected at load time. Each rule must specify at least one of `weekdays` or `hours`.

Both fields use the same `"start-end"` range format as `priority[].weekdays` / `priority[].hours`. `weekdays` are inclusive (0=Sun … 6=Sat); `hours` are half-open `[start, end)` in the 0–48 space so that values ≥ 24 wrap into the next calendar day (e.g. `"21-27"` covers 21:00–03:00).

Agents disabled by their schedule are transparently excluded from selection, so seher will fall back to the next eligible agent.

```jsonc
{
  "agents": [
    // Only use this agent on weekday business hours (Mon–Fri, 09:00–18:00)
    {
      "command": "claude",
      "active": {
        "weekdays": ["1-5"],
        "hours": ["9-18"]
      }
    },
    // Never use this agent overnight (21:00–03:00)
    {
      "command": "codex",
      "inactive": {
        "hours": ["21-27"]
      }
    }
  ]
}
```
