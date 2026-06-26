//! In-process MCP "tools" implementation.
//!
//! The Claude CLI talks to in-process tools by sending JSON-RPC requests
//! inside a `control_request` of subtype `mcp_message`. We implement just
//! enough of the MCP protocol to serve `tools/list` and `tools/call` from
//! a callable Rust closure registry -- no need to depend on a full MCP crate
//! when those are the only two methods exercised against an in-process
//! server.
//!
//! Workflow:
//! 1. Caller builds an [`AgentToolbox`] with one or more [`AgentTool`]s, then
//!    attaches it to [`ClaudeAgentOptions::sdk_mcp_server`].
//! 2. The transport receives a `control_request` whose payload is a JSON-RPC
//!    request (`{"jsonrpc": "2.0", "id": ..., "method": "tools/list" |
//!    "tools/call", "params": ...}`).
//! 3. [`AgentToolbox::handle`] returns a JSON-RPC response that the transport
//!    wraps into a `control_response`.
//!
//! [`ClaudeAgentOptions::sdk_mcp_server`]: crate::types::ClaudeAgentOptions

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::control::{ControlHandler, ControlRequest, ControlResponse, request_subtype};

/// Synchronous tool handler. Receives the raw JSON input the model produced
/// (validation/parsing is the handler's responsibility). `Ok(text)` becomes
/// the tool result; `Err(message)` is surfaced with `isError: true` so the
/// model can recover.
pub type ToolHandler = Arc<dyn Fn(Value) -> Result<String, String> + Send + Sync>;

/// One in-process tool: name, description, JSON Schema, and the handler that
/// runs when the model calls it.
#[derive(Clone)]
pub struct AgentTool {
    pub name: String,
    pub description: String,
    /// JSON Schema (`type: object`) describing the tool's input parameters.
    pub input_schema: Value,
    pub handler: ToolHandler,
}

impl AgentTool {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: ToolHandler,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler,
        }
    }
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .finish_non_exhaustive()
    }
}

/// A bundle of [`AgentTool`]s exposed to the CLI as one MCP "server".
///
/// The `name` is what appears in `--mcp-config`: `{"<name>": {"type": "sdk",
/// "name": "<name>"}}`. The tools registered here are reachable through that
/// server name (the CLI prefixes tools internally as `mcp__<name>__<tool>`).
#[derive(Clone, Debug)]
pub struct AgentToolbox {
    pub name: String,
    pub version: String,
    pub tools: Vec<AgentTool>,
}

impl AgentToolbox {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0.1.0".into(),
            tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    #[must_use]
    pub fn with_tools(mut self, tools: Vec<AgentTool>) -> Self {
        self.tools = tools;
        self
    }

    pub fn add(&mut self, tool: AgentTool) -> &mut Self {
        self.tools.push(tool);
        self
    }

    /// Process a single JSON-RPC request (what arrives inside an
    /// `mcp_message` control request).
    ///
    /// Returns a JSON-RPC response object -- already including `jsonrpc`,
    /// `id`, and either `result` or `error`. The transport wraps this in a
    /// `control_response`.
    #[must_use]
    pub fn handle_jsonrpc(&self, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => self.respond_initialize(&id),
            "tools/list" => self.respond_tools_list(&id),
            "tools/call" => self.respond_tools_call(&id, &params),
            "notifications/initialized" | "ping" => {
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            }
            other => jsonrpc_error(&id, -32601, &format!("method not found: {other}")),
        }
    }

    fn respond_initialize(&self, id: &Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {"name": self.name, "version": self.version},
                "capabilities": {"tools": {}},
            },
        })
    }

    fn respond_tools_list(&self, id: &Value) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        json!({"jsonrpc": "2.0", "id": id.clone(), "result": {"tools": tools}})
    }

    fn respond_tools_call(&self, id: &Value, params: &Value) -> Value {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let Some(tool) = self.tools.iter().find(|t| t.name == name) else {
            return jsonrpc_error(id, -32602, &format!("unknown tool: {name}"));
        };
        let (text, is_error) = match (tool.handler)(arguments) {
            Ok(s) => (s, false),
            Err(msg) => (msg, true),
        };
        json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {
                "content": [{"type": "text", "text": text}],
                "isError": is_error,
            },
        })
    }
}

fn jsonrpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": code, "message": message}})
}

/// [`ControlHandler`] that routes `mcp_message` requests to a single toolbox.
///
/// Other control subtypes (`can_use_tool`, `hook_callback`, ...) are rejected
/// with `ControlResponse::Error` since we don't model them yet.
pub struct ToolboxControlHandler {
    toolbox: Arc<AgentToolbox>,
}

impl ToolboxControlHandler {
    #[must_use]
    pub fn new(toolbox: AgentToolbox) -> Self {
        Self {
            toolbox: Arc::new(toolbox),
        }
    }
}

#[async_trait]
impl ControlHandler for ToolboxControlHandler {
    async fn handle(&self, request: ControlRequest) -> ControlResponse {
        let body = &request.request;
        match request_subtype(body) {
            "mcp_message" => {
                let msg = body.get("message").cloned().unwrap_or(Value::Null);
                let result = self.toolbox.handle_jsonrpc(&msg);
                ControlResponse::Success(json!({"mcp_response": result}))
            }
            other => ControlResponse::Error(format!("unsupported control subtype: {other}")),
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

    fn echo_tool() -> AgentTool {
        AgentTool::new(
            "echo",
            "Echo input back",
            json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
            Arc::new(|input| {
                let msg = input
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(format!("echo:{msg}"))
            }),
        )
    }

    #[test]
    fn tools_list_returns_registered_tools() {
        let tb = AgentToolbox::new("test").with_tools(vec![echo_tool()]);
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = tb.handle_jsonrpc(&req);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[0]["description"], "Echo input back");
    }

    #[test]
    fn tools_call_runs_handler() {
        let tb = AgentToolbox::new("test").with_tools(vec![echo_tool()]);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"msg": "hi"}}
        });
        let resp = tb.handle_jsonrpc(&req);
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(resp["result"]["content"][0]["text"], "echo:hi");
    }

    #[test]
    fn tools_call_unknown_returns_error_object() {
        let tb = AgentToolbox::new("test").with_tools(vec![echo_tool()]);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "missing", "arguments": {}}
        });
        let resp = tb.handle_jsonrpc(&req);
        assert_eq!(resp["error"]["code"], -32602);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing")
        );
    }

    #[test]
    fn handler_error_propagates_as_is_error() {
        let tb = AgentToolbox::new("test").with_tools(vec![AgentTool::new(
            "boom",
            "always fails",
            json!({"type": "object"}),
            Arc::new(|_| Err("nope".to_string())),
        )]);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "boom", "arguments": {}}
        });
        let resp = tb.handle_jsonrpc(&req);
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(resp["result"]["content"][0]["text"], "nope");
    }

    #[test]
    fn initialize_returns_server_info() {
        let tb = AgentToolbox::new("svr").with_version("0.2.0");
        let req = json!({"jsonrpc": "2.0", "id": 5, "method": "initialize"});
        let resp = tb.handle_jsonrpc(&req);
        assert_eq!(resp["result"]["serverInfo"]["name"], "svr");
        assert_eq!(resp["result"]["serverInfo"]["version"], "0.2.0");
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let tb = AgentToolbox::new("t");
        let req = json!({"jsonrpc": "2.0", "id": 6, "method": "weird"});
        let resp = tb.handle_jsonrpc(&req);
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn toolbox_handler_wraps_mcp_message() {
        let tb = AgentToolbox::new("t").with_tools(vec![echo_tool()]);
        let handler = ToolboxControlHandler::new(tb);
        let req = ControlRequest {
            request_id: "r1".into(),
            request: json!({
                "subtype": "mcp_message",
                "server_name": "t",
                "message": {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                }
            }),
        };
        let resp = handler.handle(req).await;
        match resp {
            ControlResponse::Success(v) => {
                assert_eq!(v["mcp_response"]["result"]["tools"][0]["name"], "echo");
            }
            ControlResponse::Error(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn toolbox_handler_rejects_other_subtypes() {
        let handler = ToolboxControlHandler::new(AgentToolbox::new("t"));
        let req = ControlRequest {
            request_id: "r2".into(),
            request: json!({"subtype": "can_use_tool"}),
        };
        let resp = handler.handle(req).await;
        assert!(matches!(resp, ControlResponse::Error(_)));
    }
}
