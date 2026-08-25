---
name: seher-config
description: Create, edit, review, and troubleshoot Seher YAML configuration files. Use this skill whenever a request mentions Seher configuration, `config.yaml`, `SEHER_CONFIG`, `--config`, providers, model routing, SDK backends, priorities, retry policy, environment variables, skill discovery, or reasoning effort--even when the user only asks to add, remove, switch, or debug one provider/model. Read the repository's schema and runtime loader before changing YAML, preserve unrelated settings and secrets, and validate the resolved provider/model selection.
---

# Seher configuration

Manage the YAML consumed by the Seher CLI. Do not edit `.claude/settings.json`, agent skills, or source code unless the user explicitly asks for those files.

## 1. Resolve the target file

Use this precedence, matching Seher's loader:

1. An explicit path in the request or `--config/-c` command.
2. The `SEHER_CONFIG` environment variable.
3. `~/.config/seher/config.yaml` when it already exists.

If no target exists:

- For a project-local config or example, create the path the user names (otherwise `./config.yaml`) and report that callers must pass `--config`.
- For a usable default user config, create `~/.config/seher/config.yaml` and its parent directory only when the user clearly requests the default config.
- Never silently replace an existing config or write to the home directory just because the request is ambiguous.

State the exact path before editing. Read the existing file first when it exists.

## 2. Read runtime truth before editing

From the repository root, inspect these sources as needed:

- `schemas/settings.schema.json` for editor-facing shape and constraints.
- `README.md` under `## Configuration` for user-facing behavior and examples.
- `crates/seher-sdk/src/sdk/config.rs` and `config_loader.rs` when schema/docs conflict or when validating precedence/defaults.

Runtime Rust types, the loader, and the checked-in JSON Schema should agree. If they conflict, inspect `crates/seher-sdk/src/sdk/config.rs` and `config_loader.rs` and report the mismatch before changing behavior.

## 3. Translate the request

Identify only the requested changes:

- provider map key and optional explicit `provider` name;
- `sdk`: `pi`, `omp`, `pi-rust`, `claude`, `claude-terminal`, or `claude-headless` in this Rust build;
- provider/model mode keys (`plan`, `build`, or a custom key selected with `--model/-m`);
- model id, optional model priority, and optional effort;
- provider priority;
- `api.key` / `api.endpoint`;
- root/provider `env`;
- root/provider `skills.includeClaude`;
- root/provider retry policy;
- root/provider/model effort.

Do not guess provider names, model ids, API endpoints, priorities, or credentials. If a required value is missing and cannot be inferred from the existing file, stop before writing and report the missing value instead of creating a placeholder config.

## 4. Edit rules

- Preserve unrelated providers, modes, comments, ordering, and formatting. Make the smallest surgical YAML edit.
- Keep at least one model under every provider. A provider with no models fails Seher's loader validation.
- A model value may be a bare string or an object:

  ```yaml
  build: provider/model
  plan:
    model: provider/model
    priority: 10
    effort: high
  ```

- Effective priority is model priority, then provider priority, then `0`. Higher priority wins; ties use provider order in the YAML map.
- A provider's `provider` field overrides its map key. Omitted `sdk` means `pi`.
- `retry` at provider level replaces the root retry block; it does not merge field-by-field. Include every non-default provider retry field that must remain active.
- `env` merges root values first, then provider values override matching keys.
- `skills.includeClaude` resolves provider override, then root value, then `true`.
- Configured effort resolves model field, then provider field, then root field. A CLI `--effort` overrides config for that run.
- Prefer existing environment credentials over `api.key`. Do not add or echo secrets unless explicitly requested. Never include secret values in the final report.
- If generating a new file, add the repository schema URI as `$schema` only when it is useful for editor validation. Do not add unrelated defaults.

Minimal shape, only after real values are known:

```yaml
$schema: https://raw.githubusercontent.com/smartcrabai/seher/main/schemas/settings.schema.json
providers:
  <provider-key>:
    models:
      <plan-or-build>: <provider/model>
```

The Rust build executes only the six SDK kinds listed above. Configs may contain portable `codex`, `copilot`, `cursor`, `kimi`, or `opencode` entries, but this build filters them out; report that clearly instead of claiming they are active.

## 5. Validate without running an agent

Run the bundled JSON Schema validator first:

```sh
uv run skills/seher-config/scripts/validate_config_schema.py <config-path>
```

The script declares its isolated `PyYAML` and `jsonschema` dependencies inline, parses YAML with `yaml.safe_load`, validates the schema itself as Draft 2020-12, and reports all instance errors without printing config values. It searches for `schemas/settings.schema.json` from the repository root; pass `--schema <path>` when working outside this repository. If `uv` is unavailable, run the script with a Python environment that already provides `PyYAML` and `jsonschema`.

Schema validation checks the document shape and constraints. It does not replace Seher's loader or routing checks; run the resolution-only command below afterward.

After editing, validate the exact file and every changed mode key with Seher's resolution-only command:

```sh
BIN=target/debug/seher
if [ ! -x "$BIN" ]; then
  cargo build -q -p seher-cli
fi
"$BIN" --show-resolution --config <config-path> --model <mode-key>
```

Use `--model` for the mode key, not the provider's model id. For the normal modes, validate both `plan` and `build` when both changed. For a custom key, pass that key explicitly. The command performs YAML loading, provider/model validation, priority resolution, SDK filtering, and reports the selected provider/model/SDK without executing a prompt.

If the binary reports no candidates, distinguish intentional configuration from a broken edit: unsupported SDKs are skipped, and a mode with no model cannot resolve. Fix malformed YAML, empty model ids, missing models, and unintended priority/order changes. Do not hide loader errors by changing the command or suppressing output.

If schema validation fails, fix the reported structural error before interpreting resolution output. The schema now covers root, provider, and model `effort` values: `low`, `medium`, `high`, `xhigh`, and `max`.

## 6. Report

Return:

- exact file path changed or created;
- concise list of requested changes;
- effective resolution for each validated mode: provider, SDK, model, priority, and effort when present;
- validation command/result;
- only actionable warnings, such as unsupported SDKs, missing environment credentials, schema/runtime drift, or an intentionally unresolved mode.

Never print API keys or other secret values.
