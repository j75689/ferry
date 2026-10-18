//! A minimal Agent Client Protocol client over the agent's stdio.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use super::install::Installed;

pub const PROTOCOL_VERSION: u64 = 1;
/// JSON-RPC error code agents use when the user must sign in first.
pub const AUTH_REQUIRED: i64 = -32000;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// The error's `data`, which often holds the agent's real reason.
    pub data: Option<String>,
}

/// JSON-RPC "internal error", what the agent reports for its own failures.
pub const INTERNAL_ERROR: i64 = -32603;
const MAX_DATA_CHARS: usize = 500;

impl RpcError {
    fn local(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Whether the agent process itself failed (it errored internally, timed
    /// out, or exited), so a fresh process may succeed.
    pub fn is_agent_failure(&self) -> bool {
        matches!(self.code, INTERNAL_ERROR | -32098 | -32099)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)?;
        match &self.data {
            Some(data) => write!(f, ": {data}"),
            None => Ok(()),
        }
    }
}

fn error_data(data: Option<&Value>) -> Option<String> {
    let text = match data? {
        Value::Null => return None,
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut short: String = text.chars().take(MAX_DATA_CHARS).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    Some(short)
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>;
type Sessions = Mutex<HashMap<String, Session>>;

/// A session Ferry is reading updates for.
struct Session {
    updates: mpsc::UnboundedSender<Value>,
    /// Names of the client tools exposed to it through Ferry's MCP server.
    tools: Vec<String>,
}

/// A running agent process plus its `initialize` result.
pub struct Connection {
    stdin: tokio::sync::Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Arc<Pending>,
    sessions: Arc<Sessions>,
    alive: Arc<AtomicBool>,
    child: Mutex<Option<Child>>,
    initialize: std::sync::OnceLock<Value>,
}

impl Connection {
    /// Start the agent and complete the `initialize` handshake.
    pub async fn spawn(
        installed: &Installed,
        startup_timeout: Duration,
    ) -> Result<Arc<Self>, String> {
        let mut command = Command::new(&installed.command);
        command
            .args(&installed.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = installed.command.parent() {
            command.current_dir(dir);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", installed.command.display()))?;
        let stdin = child.stdin.take().ok_or("agent has no stdin")?;
        let stdout = child.stdout.take().ok_or("agent has no stdout")?;
        let stderr = child.stderr.take().ok_or("agent has no stderr")?;

        let pending: Arc<Pending> = Arc::default();
        let sessions: Arc<Sessions> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn({
            let pending = pending.clone();
            let sessions = sessions.clone();
            let alive = alive.clone();
            async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        tracing::debug!(target: "ferry::acp", "agent stdout: {line}");
                        continue;
                    };
                    dispatch(message, &pending, &sessions, &reply_tx);
                }
                alive.store(false, Ordering::SeqCst);
                for (_, waiter) in pending.lock().unwrap().drain() {
                    let _ = waiter.send(Err(RpcError::local(-32099, "agent process exited")));
                }
                sessions.lock().unwrap().clear();
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "ferry::acp", "agent: {line}");
            }
        });

        let connection = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending,
            sessions,
            alive,
            child: Mutex::new(Some(child)),
            initialize: std::sync::OnceLock::new(),
        });
        // Replies to agent-initiated requests go out through the same stdin.
        tokio::spawn({
            let connection = Arc::downgrade(&connection);
            async move {
                while let Some(reply) = reply_rx.recv().await {
                    let Some(connection) = connection.upgrade() else {
                        break;
                    };
                    let _ = connection.write_line(&reply).await;
                }
            }
        });

        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "ferry", "version": env!("CARGO_PKG_VERSION")},
        });
        let initialize = connection
            .request("initialize", params, startup_timeout)
            .await
            .map_err(|e| format!("agent did not initialize: {e}"))?;
        let _ = connection.initialize.set(initialize);
        Ok(connection)
    }

    /// The agent's `initialize` result.
    pub fn initialize(&self) -> &Value {
        static EMPTY: Value = Value::Null;
        self.initialize.get().unwrap_or(&EMPTY)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(err) = self.write_line(&message.to_string()).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(RpcError::local(
                -32099,
                format!("cannot write to agent: {err}"),
            ));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::local(-32099, "agent process exited")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(RpcError::local(
                    -32098,
                    format!("{method} timed out after {}s", timeout.as_secs()),
                ))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.write_line(&message.to_string()).await;
    }

    /// Receive `session/update` payloads for `session_id` until unsubscribed.
    /// `tools` are the client tools the session reaches through Ferry's MCP
    /// server; the agent may call those when it asks for permission.
    pub fn subscribe(
        &self,
        session_id: &str,
        tools: Vec<String>,
    ) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string(), Session { updates: tx, tools });
        rx
    }

    pub fn unsubscribe(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }

    pub fn auth_methods(&self) -> Vec<Value> {
        self.initialize()["authMethods"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn capability(&self, pointer: &str) -> Option<&Value> {
        self.initialize()
            .pointer(&format!("/agentCapabilities{pointer}"))
            .filter(|v| !v.is_null() && *v != &Value::Bool(false))
    }

    pub async fn shutdown(&self) {
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
    }
}

fn dispatch(
    message: Value,
    pending: &Pending,
    sessions: &Sessions,
    replies: &mpsc::UnboundedSender<String>,
) {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned();
    match (method, id) {
        // Response to one of our requests.
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            let Some(waiter) = pending.lock().unwrap().remove(&id) else {
                return;
            };
            let result = match message.get("error") {
                Some(error) => Err(RpcError {
                    code: error["code"].as_i64().unwrap_or(INTERNAL_ERROR),
                    message: error["message"]
                        .as_str()
                        .unwrap_or("agent error")
                        .to_string(),
                    data: error_data(error.get("data")),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(result);
        }
        // Agent-initiated request: Ferry never lets the agent act on its own.
        (Some(method), Some(id)) => {
            let reply = match method {
                "session/request_permission" => {
                    let params = &message["params"];
                    let tools = params["sessionId"]
                        .as_str()
                        .and_then(|id| sessions.lock().unwrap().get(id).map(|s| s.tools.clone()))
                        .unwrap_or_default();
                    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": decide(params, &tools)}})
                }
                _ => json!({"jsonrpc": "2.0", "id": id,
                            "error": {"code": -32601, "message": format!("{method} is not supported by Ferry")}}),
            };
            let _ = replies.send(reply.to_string());
        }
        (Some("session/update"), None) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let Some(session_id) = params["sessionId"].as_str() else {
                return;
            };
            if let Some(session) = sessions.lock().unwrap().get(session_id) {
                let _ = session.updates.send(params);
            }
        }
        _ => {}
    }
}

/// Answer a permission request. Calling one of Ferry's own MCP tools only
/// hands the call back to the client, which applies its own permission
/// checks, so those are allowed once. Anything else is the agent's built-in
/// tooling and is rejected.
fn decide(params: &Value, tools: &[String]) -> Value {
    let tool_call = &params["toolCall"];
    if let Some(tool) = ferry_tool(tool_call, tools) {
        if let Some(outcome) = pick(params, &["allow_once", "allow_always"]) {
            tracing::debug!(tool, "allowed the agent to call a client tool");
            return outcome;
        }
    }
    tracing::info!(
        title = tool_call["title"].as_str().unwrap_or(""),
        kind = tool_call["kind"].as_str().unwrap_or(""),
        "rejected a permission request from the agent"
    );
    pick(params, &["reject_once", "reject_always"])
        .unwrap_or_else(|| json!({"outcome": "cancelled"}))
}

/// Select the first option of one of `kinds`, in order.
fn pick(params: &Value, kinds: &[&str]) -> Option<Value> {
    let options = params["options"].as_array()?;
    kinds.iter().find_map(|kind| {
        options
            .iter()
            .find(|o| o["kind"] == *kind)
            .and_then(|o| o["optionId"].as_str())
            .map(|id| json!({"outcome": "selected", "optionId": id}))
    })
}

/// The client tool a permission request is for, when the agent names it as
/// a tool of Ferry's MCP server (`mcp_ferry_Bash`, `Bash (ferry MCP Server)`,
/// `ferry/Bash`, ...) or by its bare name.
fn ferry_tool<'a>(tool_call: &Value, tools: &'a [String]) -> Option<&'a str> {
    let mut names: Vec<&str> = ["title", "name", "toolName"]
        .iter()
        .filter_map(|key| tool_call[*key].as_str())
        .collect();
    if let Some(meta) = tool_call["_meta"].as_object() {
        names.extend(meta.values().filter_map(Value::as_str));
    }
    tools.iter().map(String::as_str).find(|tool| {
        names.iter().map(|n| n.trim()).any(|name| {
            name == *tool
                || [
                    "mcp_ferry_",
                    "mcp__ferry__",
                    "ferry__",
                    "ferry_",
                    "ferry/",
                    "ferry.",
                    "ferry:",
                ]
                .iter()
                .any(|prefix| name.strip_prefix(prefix) == Some(tool))
                || name
                    .strip_prefix(tool)
                    .is_some_and(|rest| rest.trim_start().starts_with("(ferry"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(title: &str) -> Value {
        json!({"sessionId": "s", "toolCall": {"toolCallId": "t", "title": title, "kind": "other"},
        "options": [
            {"optionId": "a", "kind": "allow_once", "name": "Allow"},
            {"optionId": "r", "kind": "reject_once", "name": "Reject"}
        ]})
    }

    #[test]
    fn client_tools_are_allowed() {
        let tools = vec!["Bash".to_string(), "Read".to_string()];
        let allow = json!({"outcome": "selected", "optionId": "a"});
        for title in [
            "Bash",
            "mcp_ferry_Bash",
            "ferry__Read",
            "Read (ferry MCP Server)",
            "ferry/Bash",
        ] {
            assert_eq!(decide(&request(title), &tools), allow, "{title}");
        }
        let mut meta = request("Calling a tool");
        meta["toolCall"]["_meta"] = json!({"toolName": "mcp_ferry_Read"});
        assert_eq!(decide(&meta, &tools), allow);
    }

    #[test]
    fn other_permission_requests_are_rejected() {
        let tools = vec!["Bash".to_string()];
        let reject = json!({"outcome": "selected", "optionId": "r"});
        for title in [
            "run_command",
            "Run git status",
            "mcp_other_Bash",
            "Bash2",
            "BashOutput (other)",
        ] {
            assert_eq!(decide(&request(title), &tools), reject, "{title}");
        }
        assert_eq!(decide(&request("Bash"), &[]), reject);
        let no_allow = json!({"toolCall": {"title": "Bash"}, "options": [{"optionId": "r", "kind": "reject_once"}]});
        assert_eq!(decide(&no_allow, &tools), reject);
        assert_eq!(
            decide(&json!({"options": []}), &tools),
            json!({"outcome": "cancelled"})
        );
    }
}
