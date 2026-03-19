//! Web-based configuration editor served at a local HTTP port.
//!
//! Start with `seher --gui-config`. A browser window opens automatically.
//! Changes are held in memory until "Save to Disk" is clicked.

#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use axum::{
    Router,
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

use crate::config::{AgentConfig, ProviderConfig, Settings};

// ── shared state ──────────────────────────────────────────────────────────────

struct AppState {
    settings: Mutex<Settings>,
    config_path: Option<PathBuf>,
}

type SharedState = Arc<AppState>;
type HandlerResult = Result<Html<String>, (StatusCode, String)>;

fn lock_settings(
    state: &AppState,
) -> Result<std::sync::MutexGuard<'_, Settings>, (StatusCode, String)> {
    state
        .settings
        .lock()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Union of all models-map keys across agents, sorted, with "(none)" appended.
fn collect_model_keys(settings: &Settings) -> Vec<String> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for agent in &settings.agents {
        if let Some(models) = &agent.models {
            for key in models.keys() {
                keys.insert(key.clone());
            }
        }
    }
    for rule in &settings.priority {
        if let Some(model) = &rule.model {
            keys.insert(model.clone());
        }
    }
    let mut result: Vec<String> = keys.into_iter().collect();
    result.push("(none)".to_string());
    result
}

/// Priority of agent×model. Returns `None` when the model is unavailable.
fn priority_value(settings: &Settings, agent: &AgentConfig, model_key: &str) -> Option<i32> {
    if model_key == "(none)" {
        return Some(settings.priority_for(agent, None));
    }
    match &agent.models {
        Some(models) if models.contains_key(model_key) => {
            Some(settings.priority_for(agent, Some(model_key)))
        }
        Some(_) => None,
        None => Some(settings.priority_for(agent, Some(model_key))), // passthrough
    }
}

fn percent_encode_query(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b => {
                result.push_str(&format!("%{b:02X}"));
            }
        }
    }
    result
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn fmt_vec(v: &[String]) -> String {
    v.join("\n")
}

fn fmt_map(m: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join("\n")
}

fn fmt_arg_maps(m: &HashMap<String, Vec<String>>) -> String {
    let mut pairs: Vec<String> = m
        .iter()
        .map(|(k, v)| format!("{k}={}", v.join(" ")))
        .collect();
    pairs.sort();
    pairs.join("\n")
}

fn provider_display(agent: &AgentConfig) -> String {
    match &agent.provider {
        None | Some(ProviderConfig::Inferred) => String::new(),
        Some(ProviderConfig::Explicit(s)) => s.clone(),
        Some(ProviderConfig::None) => "null".to_string(),
    }
}

// ── form parsers ──────────────────────────────────────────────────────────────

fn parse_vec(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn parse_map(s: &str) -> HashMap<String, String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn parse_arg_maps(s: &str) -> HashMap<String, Vec<String>> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let (k, rest) = l.split_once('=')?;
            let vals: Vec<String> = rest.split_whitespace().map(String::from).collect();
            Some((k.trim().to_string(), vals))
        })
        .collect()
}

fn non_empty_map<K, V>(m: HashMap<K, V>) -> Option<HashMap<K, V>> {
    if m.is_empty() { None } else { Some(m) }
}

fn parse_provider(s: &str) -> Option<ProviderConfig> {
    match s.trim() {
        "" => None,
        "null" => Some(ProviderConfig::None),
        other => Some(ProviderConfig::Explicit(other.to_string())),
    }
}

// ── HTML rendering ─────────────────────────────────────────────────────────────

struct AgentDisplay {
    command: String,
    provider: String,
    args: String,
    models_str: String,
    env_str: String,
    pre_cmd: String,
    arg_maps_str: String,
}

impl AgentDisplay {
    fn new(agent: &AgentConfig) -> Self {
        Self {
            command: escape_html(&agent.command),
            provider: escape_html(&provider_display(agent)),
            args: escape_html(&fmt_vec(&agent.args)),
            models_str: agent
                .models
                .as_ref()
                .map_or_else(String::new, |m| escape_html(&fmt_map(m))),
            env_str: agent
                .env
                .as_ref()
                .map_or_else(String::new, |e| escape_html(&fmt_map(e))),
            pre_cmd: escape_html(&fmt_vec(&agent.pre_command)),
            arg_maps_str: escape_html(&fmt_arg_maps(&agent.arg_maps)),
        }
    }
}

fn render_agent_row(
    idx: usize,
    agent: &AgentConfig,
    settings: &Settings,
    model_keys: &[String],
) -> String {
    let AgentDisplay { command, provider, args, models_str, env_str, pre_cmd, arg_maps_str } =
        AgentDisplay::new(agent);

    let priority_cells: String = model_keys
        .iter()
        .map(|mk| match priority_value(settings, agent, mk) {
            None => r#"<td class="unavail">—</td>"#.to_string(),
            Some(p) => format!(r#"<td class="prio">{p}</td>"#),
        })
        .collect();

    format!(
        r##"<tr id="agent-row-{idx}">
  <td><span class="cmd-chip">{command}</span></td>
  <td><span class="prov-text">{provider}</span></td>
  <td><pre>{args}</pre></td>
  <td><pre>{models_str}</pre></td>
  <td><pre>{env_str}</pre></td>
  <td><pre>{pre_cmd}</pre></td>
  <td><pre>{arg_maps_str}</pre></td>
  {priority_cells}
  <td class="actions"><div class="actions-wrap">
    <button class="btn-edit" hx-get="/agents/{idx}/edit" hx-target="#agent-row-{idx}" hx-swap="outerHTML">Edit</button>
    <button class="btn-del" hx-delete="/agents/{idx}" hx-target="#agents-body" hx-swap="innerHTML">Del</button>
  </div></td>
</tr>"##
    )
}

fn render_edit_row(
    idx: usize,
    agent: &AgentConfig,
    settings: &Settings,
    model_keys: &[String],
) -> String {
    let AgentDisplay { command, provider, args, models_str, env_str, pre_cmd, arg_maps_str } =
        AgentDisplay::new(agent);

    let priority_inputs: String = model_keys
        .iter()
        .map(|mk| match priority_value(settings, agent, mk) {
            None => r#"<td class="unavail">-</td>"#.to_string(),
            Some(p) => {
                let p_str = if p == 0 {
                    String::new()
                } else {
                    p.to_string()
                };
                let safe_mk = escape_html(mk);
                format!(
                    "<td><input name=\"p_{safe_mk}\" value=\"{p_str}\" placeholder=\"0\"></td>"
                )
            }
        })
        .collect();

    format!(
        r##"<tr id="agent-row-{idx}" class="editing">
  <td><input name="command" value="{command}" placeholder="command"></td>
  <td><input name="provider" value="{provider}" placeholder="(inferred)"></td>
  <td><textarea name="args" rows="3">{args}</textarea></td>
  <td><textarea name="models" rows="3">{models_str}</textarea></td>
  <td><textarea name="env" rows="3">{env_str}</textarea></td>
  <td><textarea name="pre_command" rows="3">{pre_cmd}</textarea></td>
  <td><textarea name="arg_maps" rows="3">{arg_maps_str}</textarea></td>
  {priority_inputs}
  <td class="actions"><div class="actions-wrap">
    <button class="btn-save" hx-put="/agents/{idx}" hx-include="closest tr" hx-target="#agents-body" hx-swap="innerHTML">Save</button>
    <button class="btn-cancel" hx-get="/agents/{idx}" hx-target="#agent-row-{idx}" hx-swap="outerHTML">Cancel</button>
  </div></td>
</tr>"##
    )
}

fn render_tbody(settings: &Settings, model_keys: &[String]) -> String {
    settings
        .agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| render_agent_row(idx, agent, settings, model_keys))
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(clippy::format_collect)]
fn render_thead_model_cols(model_keys: &[String], sort_by: Option<&str>) -> String {
    model_keys
        .iter()
        .map(|mk| {
            let is_sorted = sort_by == Some(mk.as_str());
            let class = if is_sorted { r#" class="th-sorted""# } else { "" };
            let marker = if is_sorted { " ↓" } else { "" };
            let encoded_mk = percent_encode_query(mk);
            let escaped_mk = escape_html(mk);
            format!("<th{class}><a href=\"/?sort={encoded_mk}\">{escaped_mk}{marker}</a></th>")
        })
        .collect()
}

fn render_full_page(settings: &Settings, model_keys: &[String], sort_by: Option<&str>) -> String {
    let mut indexed: Vec<(usize, &AgentConfig)> = settings.agents.iter().enumerate().collect();
    if let Some(sk) = sort_by {
        indexed.sort_by_key(|(_, a)| {
            std::cmp::Reverse(priority_value(settings, a, sk).unwrap_or(i32::MIN))
        });
    }

    let thead_model_cols = render_thead_model_cols(model_keys, sort_by);
    let tbody: String = indexed
        .iter()
        .map(|(idx, agent)| render_agent_row(*idx, agent, settings, model_keys))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>seher config</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link href="https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;500;600&family=Fira+Code:wght@400;500&display=swap" rel="stylesheet">
  <script src="https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js"></script>
  <style>
    :root {{
      --bg:      #060b14;
      --s0:      #091220;
      --s1:      #0d1929;
      --s2:      #121f32;
      --bd:      #1a2d42;
      --bd2:     #253d58;
      --t0:      #7a9ab8;
      --t1:      #c0d4e8;
      --t2:      #e0eeff;
      --teal:    #00c9a7;
      --teal-d:  rgba(0,201,167,.12);
      --amber:   #ffc857;
      --amber-d: rgba(255,200,87,.1);
      --red:     #ff5a5f;
      --red-d:   rgba(255,90,95,.1);
      --green:   #05d69e;
      --green-d: rgba(5,214,158,.1);
    }}
    *, *::before, *::after {{ box-sizing: border-box; margin: 0; padding: 0; }}
    html {{ scroll-behavior: smooth; }}
    body {{
      font-family: 'Outfit', system-ui, sans-serif;
      background: var(--bg);
      color: var(--t1);
      min-height: 100vh;
      display: flex;
      flex-direction: column;
      overflow-x: auto;
    }}

    /* ── Header ─────────────────────────────────────────── */
    .header {{
      position: sticky; top: 0; z-index: 50;
      background: rgba(6,11,20,.9);
      backdrop-filter: blur(14px);
      border-bottom: 1px solid var(--bd);
      display: flex; align-items: center;
      padding: 0 1.5rem; height: 52px; gap: 1rem;
    }}
    .logo {{
      font-family: 'Fira Code', monospace;
      font-weight: 500; font-size: .95rem;
      color: var(--teal); letter-spacing: .05em;
      display: flex; align-items: center; gap: .55rem;
    }}
    .logo-dot {{
      width: 7px; height: 7px; border-radius: 50%;
      background: var(--teal);
      box-shadow: 0 0 6px var(--teal);
      animation: blink 2.4s ease-in-out infinite;
    }}
    @keyframes blink {{
      0%,100% {{ opacity:1; box-shadow: 0 0 6px var(--teal); }}
      50% {{ opacity:.45; box-shadow: 0 0 2px var(--teal); }}
    }}
    .logo-sep {{ color: var(--bd2); margin: 0 .1rem; }}
    .header-label {{
      font-size: .68rem; font-weight: 400;
      text-transform: uppercase; letter-spacing: .13em;
      color: var(--t0);
    }}
    .header-right {{
      margin-left: auto; display: flex;
      align-items: center; gap: .75rem;
    }}

    /* ── Buttons ─────────────────────────────────────────── */
    button {{
      cursor: pointer;
      font-family: 'Outfit', sans-serif; font-weight: 500;
      border-radius: 5px; border: 1px solid transparent;
      transition: all .15s ease; line-height: 1; white-space: nowrap;
    }}
    .btn-primary {{
      padding: .38rem 1.05rem; font-size: .8rem;
      background: var(--amber); color: #1a0d00; border-color: var(--amber);
    }}
    .btn-primary:hover {{
      background: #ffd47a;
      box-shadow: 0 0 18px rgba(255,200,87,.4);
    }}
    .btn-edit {{
      padding: .22rem .62rem; font-size: .7rem;
      color: var(--teal); background: var(--teal-d);
      border-color: rgba(0,201,167,.28);
    }}
    .btn-edit:hover {{
      background: rgba(0,201,167,.2); border-color: var(--teal);
      box-shadow: 0 0 8px rgba(0,201,167,.2);
    }}
    .btn-del {{
      padding: .22rem .62rem; font-size: .7rem;
      color: var(--red); background: var(--red-d);
      border-color: rgba(255,90,95,.28);
    }}
    .btn-del:hover {{
      background: rgba(255,90,95,.2); border-color: var(--red);
    }}
    .btn-save {{
      padding: .22rem .62rem; font-size: .7rem;
      color: var(--green); background: var(--green-d);
      border-color: rgba(5,214,158,.28);
    }}
    .btn-save:hover {{
      background: rgba(5,214,158,.2); border-color: var(--green);
    }}
    .btn-cancel {{
      padding: .22rem .62rem; font-size: .7rem;
      color: var(--t0); background: transparent; border-color: var(--bd);
    }}
    .btn-cancel:hover {{ color: var(--t1); border-color: var(--bd2); background: var(--s1); }}
    .btn-add {{
      padding: .38rem 1.1rem; font-size: .78rem;
      color: var(--amber); background: transparent;
      border: 1px dashed rgba(255,200,87,.38);
    }}
    .btn-add:hover {{ background: var(--amber-d); border-color: var(--amber); }}

    /* ── Status badge ─────────────────────────────────────── */
    #status {{
      display: inline-flex; align-items: center; gap: .35rem;
      padding: .28rem .72rem;
      background: var(--green-d); color: var(--green);
      border: 1px solid rgba(5,214,158,.28);
      border-radius: 4px; font-size: .73rem; font-weight: 500;
      opacity: 0; pointer-events: none;
      transition: opacity .2s ease;
    }}
    #status.show {{ opacity: 1; }}

    /* ── Layout ───────────────────────────────────────────── */
    .main {{ padding: 1.25rem 1.5rem; flex: 1; min-width: 0; }}
    .table-wrap {{
      border: 1px solid var(--bd); border-radius: 8px;
      overflow: auto; background: var(--s0);
    }}

    /* ── Table ────────────────────────────────────────────── */
    table {{ width: 100%; border-collapse: collapse; font-size: .77rem; }}
    thead {{
      background: var(--s2);
      position: sticky; top: 0; z-index: 10;
      border-bottom: 1px solid var(--bd2);
    }}
    th {{
      padding: .52rem .82rem;
      font-size: .63rem; font-weight: 600;
      text-transform: uppercase; letter-spacing: .11em;
      color: var(--t0); white-space: nowrap; text-align: left;
      border-right: 1px solid var(--bd);
    }}
    th:last-child {{ border-right: none; }}
    th a {{
      color: inherit; text-decoration: none;
      display: inline-flex; align-items: center; gap: .25rem;
    }}
    th a:hover {{ color: var(--teal); }}
    .th-sorted {{ color: var(--teal) !important; }}
    tbody tr {{ border-bottom: 1px solid var(--bd); transition: background .1s; }}
    tbody tr:last-child {{ border-bottom: none; }}
    tbody tr:hover {{ background: rgba(255,255,255,.016); }}
    tbody tr.editing {{
      background: rgba(255,200,87,.04);
      box-shadow: inset 3px 0 0 var(--amber);
    }}
    td {{
      padding: .48rem .82rem; vertical-align: top;
      border-right: 1px solid var(--bd); color: var(--t1);
    }}
    td:last-child {{ border-right: none; }}
    td.unavail {{ color: var(--bd2); text-align: center; }}
    td.prio {{
      text-align: center;
      font-family: 'Fira Code', monospace; font-size: .7rem;
      color: var(--teal); font-weight: 500;
    }}
    td.actions {{ white-space: nowrap; }}
    .actions-wrap {{ display: flex; gap: .35rem; align-items: center; }}

    /* ── Content cells ───────────────────────────────────── */
    pre {{
      margin: 0; white-space: pre-wrap; word-break: break-all;
      font-family: 'Fira Code', monospace; font-size: .7rem;
      color: var(--t1); line-height: 1.55;
    }}
    .cmd-chip {{
      font-family: 'Fira Code', monospace; font-size: .7rem;
      background: var(--teal-d); color: var(--teal);
      border: 1px solid rgba(0,201,167,.22);
      border-radius: 4px; padding: .14rem .52rem;
      display: inline-block; white-space: nowrap;
    }}
    .prov-text {{
      font-family: 'Fira Code', monospace; font-size: .7rem; color: var(--t0);
    }}

    /* ── Inputs ─────────────────────────────────────────── */
    input, textarea {{
      background: var(--bg); border: 1px solid var(--bd);
      border-radius: 4px; color: var(--t1);
      font-family: 'Fira Code', monospace; font-size: .7rem;
      padding: .3rem .5rem; width: 100%;
      transition: border-color .15s, box-shadow .15s; resize: vertical;
    }}
    input:focus, textarea:focus {{
      outline: none; border-color: var(--amber);
      box-shadow: 0 0 0 2px rgba(255,200,87,.15);
    }}
    input[name="command"] {{ min-width: 96px; }}
    input[name="provider"] {{ min-width: 78px; }}
    input[name^="p_"] {{ width: 56px; text-align: center; }}
    textarea {{ min-width: 110px; }}

    /* ── Footer ──────────────────────────────────────────── */
    .footer {{ padding: .9rem 1.5rem; border-top: 1px solid var(--bd); }}

    /* ── Scrollbar ───────────────────────────────────────── */
    ::-webkit-scrollbar {{ width: 6px; height: 6px; }}
    ::-webkit-scrollbar-track {{ background: var(--bg); }}
    ::-webkit-scrollbar-thumb {{ background: var(--bd2); border-radius: 3px; }}
    ::-webkit-scrollbar-thumb:hover {{ background: var(--t0); }}
  </style>
</head>
<body>
  <header class="header">
    <div class="logo">
      <span class="logo-dot"></span>
      seher
    </div>
    <span class="logo-sep">/</span>
    <span class="header-label">Config Editor</span>
    <div class="header-right">
      <span id="status">Saved ✓</span>
      <button class="btn-primary"
              hx-post="/save"
              hx-target="#status"
              hx-swap="innerHTML"
              hx-on::after-request="const s=document.getElementById('status');s.classList.add('show');setTimeout(()=>s.classList.remove('show'),2600)">
        Save to Disk
      </button>
    </div>
  </header>
  <main class="main">
    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th>command</th>
            <th>provider</th>
            <th>args</th>
            <th>models</th>
            <th>env</th>
            <th>pre_command</th>
            <th>arg_maps</th>
            {thead_model_cols}
            <th>actions</th>
          </tr>
        </thead>
        <tbody id="agents-body">
          {tbody}
        </tbody>
      </table>
    </div>
  </main>
  <div class="footer">
    <button class="btn-add" hx-post="/agents" hx-target="#agents-body" hx-swap="innerHTML">
      + Add Agent
    </button>
  </div>
</body>
</html>"##
    )
}

// ── handlers ──────────────────────────────────────────────────────────────────

async fn index_handler(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> HandlerResult {
    let settings = lock_settings(&state)?;
    let sort_by = params.get("sort").map(String::as_str);
    let model_keys = collect_model_keys(&settings);
    Ok(Html(render_full_page(&settings, &model_keys, sort_by)))
}

async fn edit_agent_handler(
    State(state): State<SharedState>,
    Path(idx): Path<usize>,
) -> HandlerResult {
    let settings = lock_settings(&state)?;
    let agent = settings
        .agents
        .get(idx)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Agent not found".to_string()))?;
    let model_keys = collect_model_keys(&settings);
    Ok(Html(render_edit_row(idx, agent, &settings, &model_keys)))
}

async fn view_agent_handler(
    State(state): State<SharedState>,
    Path(idx): Path<usize>,
) -> HandlerResult {
    let settings = lock_settings(&state)?;
    let agent = settings
        .agents
        .get(idx)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Agent not found".to_string()))?;
    let model_keys = collect_model_keys(&settings);
    Ok(Html(render_agent_row(idx, agent, &settings, &model_keys)))
}

async fn update_agent_handler(
    State(state): State<SharedState>,
    Path(idx): Path<usize>,
    Form(form): Form<HashMap<String, String>>,
) -> HandlerResult {
    let mut settings = lock_settings(&state)?;

    if idx >= settings.agents.len() {
        return Err((StatusCode::NOT_FOUND, "Agent not found".to_string()));
    }

    let command = form
        .get("command")
        .map_or_else(String::new, |s| s.trim().to_string());
    let provider = parse_provider(form.get("provider").map_or("", String::as_str));
    let args = parse_vec(form.get("args").map_or("", String::as_str));
    let models = non_empty_map(parse_map(form.get("models").map_or("", String::as_str)));
    let env = non_empty_map(parse_map(form.get("env").map_or("", String::as_str)));
    let pre_command = parse_vec(form.get("pre_command").map_or("", String::as_str));
    let arg_maps = parse_arg_maps(form.get("arg_maps").map_or("", String::as_str));

    {
        let agent = &mut settings.agents[idx];
        agent.command = command;
        agent.provider = provider;
        agent.args = args;
        agent.models = models;
        agent.env = env;
        agent.pre_command = pre_command;
        agent.arg_maps = arg_maps;
    }

    // Process priority fields: "p_{model_key}" → update PriorityRules
    let agent_command = settings.agents[idx].command.clone();
    let agent_provider = settings.agents[idx].provider.clone();

    for (key, val) in &form {
        let Some(model_suffix) = key.strip_prefix("p_") else {
            continue;
        };
        let model_key: Option<String> = if model_suffix == "(none)" {
            None
        } else {
            Some(model_suffix.to_string())
        };

        let trimmed = val.trim();
        if trimmed.is_empty() || trimmed == "0" {
            settings.remove_priority(
                &agent_command,
                agent_provider.as_ref(),
                model_key.as_deref(),
            );
        } else if let Ok(p) = trimmed.parse::<i32>() {
            settings.upsert_priority(&agent_command, agent_provider.clone(), model_key, p);
        }
    }

    let model_keys = collect_model_keys(&settings);
    Ok(Html(render_tbody(&settings, &model_keys)))
}

async fn add_agent_handler(State(state): State<SharedState>) -> HandlerResult {
    let mut settings = lock_settings(&state)?;
    settings.agents.push(AgentConfig {
        command: "new-agent".to_string(),
        args: vec![],
        models: None,
        arg_maps: HashMap::new(),
        env: None,
        provider: None,
        openrouter_management_key: None,
        pre_command: vec![],
    });
    let model_keys = collect_model_keys(&settings);
    Ok(Html(render_tbody(&settings, &model_keys)))
}

async fn delete_agent_handler(
    State(state): State<SharedState>,
    Path(idx): Path<usize>,
) -> HandlerResult {
    let mut settings = lock_settings(&state)?;
    if idx >= settings.agents.len() {
        return Err((StatusCode::NOT_FOUND, "Agent not found".to_string()));
    }
    settings.agents.remove(idx);
    let model_keys = collect_model_keys(&settings);
    Ok(Html(render_tbody(&settings, &model_keys)))
}

async fn save_handler(State(state): State<SharedState>) -> HandlerResult {
    let settings = lock_settings(&state)?;
    settings
        .save(state.config_path.as_deref())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Html("Saved &#10003;".to_string()))
}

// ── entry point ───────────────────────────────────────────────────────────────

/// Start the config editor web server, open the browser, and block until Ctrl+C.
///
/// # Errors
///
/// Returns an error if the TCP listener cannot be bound or the server fails.
pub async fn serve(
    settings: Settings,
    config_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState {
        settings: Mutex::new(settings),
        config_path,
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/agents/{idx}/edit", get(edit_agent_handler))
        .route(
            "/agents/{idx}",
            get(view_agent_handler)
                .put(update_agent_handler)
                .delete(delete_agent_handler),
        )
        .route("/agents", post(add_agent_handler))
        .route("/save", post(save_handler))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    eprintln!("Config editor: http://127.0.0.1:{port}");
    let _ = open::that(format!("http://127.0.0.1:{port}"));
    axum::serve(listener, app).await?;
    Ok(())
}
