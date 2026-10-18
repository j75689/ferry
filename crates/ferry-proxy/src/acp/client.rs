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
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>;
type Sessions = Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>;

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
                    let _ = waiter.send(Err(RpcError {
                        code: -32099,
                        message: "agent process exited".into(),
                    }));
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
            return Err(RpcError {
                code: -32099,
                message: format!("cannot write to agent: {err}"),
            });
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError {
                code: -32099,
                message: "agent process exited".into(),
            }),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(RpcError {
                    code: -32098,
                    message: format!("{method} timed out after {}s", timeout.as_secs()),
                })
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.write_line(&message.to_string()).await;
    }

    /// Receive `session/update` payloads for `session_id` until unsubscribed.
    pub fn subscribe(&self, session_id: &str) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string(), tx);
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
                    code: error["code"].as_i64().unwrap_or(-32603),
                    message: error["message"]
                        .as_str()
                        .unwrap_or("agent error")
                        .to_string(),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(result);
        }
        // Agent-initiated request: Ferry never lets the agent act on its own.
        (Some(method), Some(id)) => {
            let reply = match method {
                "session/request_permission" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": reject(&message["params"])}})
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
            if let Some(tx) = sessions.lock().unwrap().get(session_id) {
                let _ = tx.send(params);
            }
        }
        _ => {}
    }
}

/// Pick the agent's own "reject" option, falling back to a cancellation.
fn reject(params: &Value) -> Value {
    let options = params["options"].as_array().cloned().unwrap_or_default();
    let choice = ["reject_once", "reject_always"].iter().find_map(|kind| {
        options
            .iter()
            .find(|o| o["kind"] == *kind)
            .and_then(|o| o["optionId"].as_str())
    });
    match choice {
        Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
        None => json!({"outcome": "cancelled"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_requests_are_rejected() {
        let params = json!({"options": [
            {"optionId": "a", "kind": "allow_once", "name": "Allow"},
            {"optionId": "r", "kind": "reject_once", "name": "Reject"}
        ]});
        assert_eq!(
            reject(&params),
            json!({"outcome": "selected", "optionId": "r"})
        );
        assert_eq!(
            reject(&json!({"options": []})),
            json!({"outcome": "cancelled"})
        );
    }
}
