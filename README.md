# Seher


Seher is a CLI tool that waits for Claude's Rate Limit to reset, then executes a specified prompt using Claude Code.


## How it works


By default, it retrieves Chrome's cookie information and uses it to call Claude's API to get the Rate Limit reset time. The browser and profile from which cookies are retrieved can be changed with options.


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


You can customize seher's behavior by creating `~/.seher/settings.json` or `~/.seher/settings.jsonc` (JSONC supports `//` comments). If neither file exists, the default configuration (using `claude` with no extra arguments) is applied.


### Settings


| Field | Type | Description |
|-------|------|-------------|
| `agents` | array | List of agents to use |
| `agents[].command` | string | Command name (`"claude"` or `"copilot"`) |
| `agents[].args` | array of strings | Additional arguments (optional) |
| `agents[].models` | object | Model level mapping (optional) |
| `agents[].arg_maps` | object | Exact-match mapping from trailing CLI tokens to replacement token arrays (optional) |
| `agents[].env` | object | Environment variables to set when running the agent (optional) |
| `agents[].provider` | string or null | Rate limit provider override (optional, see below) |


### Example


```json
{
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

`arg_maps` rewrites each trailing CLI token independently using exact-match keys. A mapping value can expand one input token into multiple output tokens, while unmapped tokens are passed through unchanged. For example, with the sample configuration, `seher --danger "fix bugs"` adds `--permission-mode bypassPermissions` when Claude is selected, or `--yolo` when Copilot is selected.

The `provider` field controls rate limit tracking. If omitted, the provider is inferred from the command name (`claude` → claude.ai, `copilot` → github.com). Setting it to `null` disables rate limit checking for that agent, making it act as an unconditional fallback. Setting it to a string (e.g. `"copilot"`) uses that provider's rate limit regardless of the command name. When multiple agents are configured, seher preferentially selects agents that are not rate-limited; agents with `provider: null` are used as a last resort.

The `env` field specifies environment variables to inject when launching the agent. This is useful for switching API keys or base URLs to route a standard command (e.g. `claude`) to a different backend.
