//! Loopback MCP server that shows the client's tools to the agent.
//!
//! The agent never runs these tools: a `tools/call` is handed back to the
//! bridge, which returns it to Claude Code / Codex as the model's tool call.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Sent to the agent as the tool's result; the real result comes next turn.
pub const FORWARDED_RESULT: &str =
    "Forwarded to the user's assistant, which runs this tool itself and \
will send the result in the next message. End your turn now without any further output.";

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

struct Bridge {
    tools: Vec<Value>,
    calls: mpsc::UnboundedSender<ToolCall>,
}

#[derive(Clone, Default)]
pub struct McpServer {
    bridges: Arc<Mutex<HashMap<String, Bridge>>>,
    addr: Arc<tokio::sync::OnceCell<SocketAddr>>,
}

/// Unregisters its tools when dropped.
pub struct Registration {
    server: McpServer,
    token: String,
    pub url: String,
}

impl Registration {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.server.bridges.lock().unwrap().remove(&self.token);
    }
}

impl McpServer {
    async fn addr(&self) -> std::io::Result<SocketAddr> {
        let server = self.clone();
        self.addr
            .get_or_try_init(|| async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
                let addr = listener.local_addr()?;
                let app = Router::new()
                    .route("/mcp", post(handle).get(no_stream).delete(ok))
                    .with_state(server);
                tokio::spawn(async move {
                    if let Err(err) = axum::serve(listener, app).await {
                        tracing::error!("MCP bridge stopped: {err}");
                    }
                });
                Ok(addr)
            })
            .await
            .copied()
    }

    /// Publish `tools` (OpenAI function definitions) under a fresh token.
    pub async fn register(
        &self,
        tools: &[Value],
    ) -> std::io::Result<(Registration, mpsc::UnboundedReceiver<ToolCall>)> {
        let addr = self.addr().await?;
        let token = crate::stream::new_id("mcp_");
        let (tx, rx) = mpsc::unbounded_channel();
        let tools = tools.iter().filter_map(to_mcp_tool).collect();
        self.bridges
            .lock()
            .unwrap()
            .insert(token.clone(), Bridge { tools, calls: tx });
        let registration = Registration {
            server: self.clone(),
            token,
            url: format!("http://{addr}/mcp"),
        };
        Ok((registration, rx))
    }
}

/// OpenAI `{type: function, function: {name, description, parameters}}` to MCP.
fn to_mcp_tool(tool: &Value) -> Option<Value> {
    let function = tool.get("function")?;
    Some(json!({
        "name": function.get("name")?.as_str()?,
        "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
        "inputSchema": function.get("parameters").cloned().unwrap_or(json!({"type": "object"})),
    }))
}

async fn no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn ok() -> StatusCode {
    StatusCode::OK
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

async fn handle(
    State(server): State<McpServer>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let messages = match body {
        Value::Array(items) => items,
        single => vec![single],
    };
    let mut replies = Vec::new();
    for message in messages {
        if let Some(reply) = respond(&server, token, &message) {
            replies.push(reply);
        }
    }
    match replies.len() {
        0 => StatusCode::ACCEPTED.into_response(),
        1 => Json(replies.remove(0)).into_response(),
        _ => Json(Value::Array(replies)).into_response(),
    }
}

fn respond(server: &McpServer, token: &str, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or("");
    let bridges = server.bridges.lock().unwrap();
    let Some(bridge) = bridges.get(token) else {
        return Some(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "unknown or expired session"}}),
        );
    };
    let result = match method {
        "initialize" => json!({
            "protocolVersion": message.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "ferry", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": bridge.tools}),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !bridge.tools.iter().any(|t| t["name"] == name) {
                return Some(json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": format!("Unknown tool {name:?}.")}], "isError": true}}));
            }
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            let _ = bridge.calls.send(ToolCall { name, arguments });
            json!({"content": [{"type": "text", "text": FORWARDED_RESULT}]})
        }
        other => {
            return Some(json!({"jsonrpc": "2.0", "id": id,
                               "error": {"code": -32601, "message": format!("{other} is not supported")}}));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}
