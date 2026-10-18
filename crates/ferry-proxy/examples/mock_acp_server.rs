//! A stand-in for Google's `agy_acp_server`, for developing and testing the
//! Antigravity integration without a Google account.
//!
//! It speaks ACP over stdio: it requires `authenticate` before sessions
//! (remembered in `$MOCK_ACP_STATE` or the system temp dir), offers two
//! models, streams a thought and a reply, and when the prompt contains
//! `RUN_TOOL` it calls the client's shell tool through the session's HTTP
//! MCP server, the way the real agent uses client-provided tools. Like the
//! real agent, it asks the client for permission before each tool call.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

fn state_file() -> PathBuf {
    std::env::var_os("MOCK_ACP_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mock-acp-signed-in"))
}

struct Session {
    model: String,
    mcp: Option<(String, String)>,
}

type Out = Arc<Mutex<std::io::Stdout>>;
type Waiting = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>;

/// Ask the client for permission to call `tool`; true when it allowed it.
async fn permitted(out: &Out, waiting: &Waiting, session: &str, tool: &str) -> bool {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (tx, rx) = tokio::sync::oneshot::channel();
    waiting.lock().unwrap().insert(id, tx);
    send(
        out,
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": session,
            "toolCall": {"toolCallId": format!("perm-{id}"), "title": format!("mcp_ferry_{tool}"), "kind": "other"},
            "options": [{"optionId": "proceed_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "cancel", "name": "Deny", "kind": "reject_once"}]}}),
    );
    let reply = rx.await.unwrap_or(Value::Null);
    reply.pointer("/result/outcome/optionId") == Some(&json!("proceed_once"))
}

fn send(out: &Out, message: Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn update(out: &Out, session: &str, update: Value) {
    send(
        out,
        json!({"jsonrpc": "2.0", "method": "session/update",
                     "params": {"sessionId": session, "update": update}}),
    );
}

fn chunk(out: &Out, session: &str, kind: &str, text: &str) {
    update(
        out,
        session,
        json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
    );
}

fn models(current: &str) -> Value {
    json!([{"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": current,
            "options": [{"value": "gemini-mock-pro", "name": "Gemini Mock Pro"},
                        {"value": "gemini-mock-flash", "name": "Gemini Mock Flash"}]}])
}

async fn call_tool(
    url: &str,
    token: &str,
    prompt: &str,
    (out, waiting, session): (&Out, &Waiting, &str),
) -> Result<(), String> {
    let http = reqwest::Client::new();
    let rpc = |id: u64, method: &str, params: Value| {
        http.post(url)
            .bearer_auth(token)
            .json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .send()
    };
    rpc(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
    )
    .await
    .map_err(|e| e.to_string())?;
    let tools: Value = rpc(2, "tools/list", json!({}))
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = tools
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    let (name, arguments) = if names.iter().any(|n| n == "Bash") {
        (
            "Bash",
            json!({"command": "echo ferry-acp-tool-ok", "description": "Mock ACP tool call"}),
        )
    } else if names.iter().any(|n| n == "exec_command") {
        ("exec_command", json!({"cmd": "echo ferry-acp-tool-ok"}))
    } else if names.iter().any(|n| n == "shell") {
        ("shell", json!({"command": ["echo", "ferry-acp-tool-ok"]}))
    } else {
        return Err(format!(
            "no shell tool among {names:?} (prompt was {} chars)",
            prompt.len()
        ));
    };
    if !permitted(out, waiting, session, name).await {
        return Err("tool call denied by pre-tool hook: Denied by user (*)".into());
    }
    rpc(
        3,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let sessions: Arc<Mutex<HashMap<String, Session>>> = Arc::default();
    let waiting: Waiting = Arc::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if let Ok(message) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(message);
            }
        }
    });
    let mut next_session = 0;
    while let Some(message) = rx.recv().await {
        let id = message["id"].clone();
        let params = message["params"].clone();
        if message.get("method").is_none() {
            // A reply to one of our own requests.
            if let Some(tx) = id
                .as_u64()
                .and_then(|id| waiting.lock().unwrap().remove(&id))
            {
                let _ = tx.send(message);
            }
            continue;
        }
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match message["method"].as_str().unwrap_or("") {
            "initialize" => send(
                &out,
                reply(json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {"mcpCapabilities": {"http": true}, "promptCapabilities": {"image": true},
                                          "sessionCapabilities": {"close": {}, "systemPrompt": {}}},
                    "authMethods": [{"id": "oauth-personal", "name": "Sign in with Google"},
                                    {"id": "gemini-api-key", "name": "Use a Gemini API key"}],
                    "agentInfo": {"name": "mock-agy-acp", "version": "0.0.1"},
                })),
            ),
            "authenticate" => {
                let _ = std::fs::write(state_file(), params["methodId"].as_str().unwrap_or(""));
                send(&out, reply(json!({})));
            }
            "session/new" => {
                if !state_file().exists() {
                    send(
                        &out,
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "Authentication required"}}),
                    );
                    continue;
                }
                next_session += 1;
                let session_id = format!("mock-session-{next_session}");
                let mcp = params["mcpServers"]
                    .as_array()
                    .and_then(|servers| servers.first())
                    .map(|s| {
                        let token = s["headers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|h| h["name"] == "Authorization")
                            .and_then(|h| h["value"].as_str())
                            .unwrap_or("")
                            .trim_start_matches("Bearer ")
                            .to_string();
                        (s["url"].as_str().unwrap_or("").to_string(), token)
                    });
                sessions.lock().unwrap().insert(
                    session_id.clone(),
                    Session {
                        model: "gemini-mock-pro".into(),
                        mcp,
                    },
                );
                send(
                    &out,
                    reply(
                        json!({"sessionId": session_id, "configOptions": models("gemini-mock-pro")}),
                    ),
                );
            }
            "session/set_config_option" => {
                let value = params["value"].as_str().unwrap_or("").to_string();
                if let Some(session) = sessions
                    .lock()
                    .unwrap()
                    .get_mut(params["sessionId"].as_str().unwrap_or(""))
                {
                    session.model = value.clone();
                }
                send(&out, reply(json!({"configOptions": models(&value)})));
            }
            "session/prompt" => {
                let session_id = params["sessionId"].as_str().unwrap_or("").to_string();
                let prompt: String = params["prompt"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect();
                let (model, mcp) = {
                    let sessions = sessions.lock().unwrap();
                    let session = sessions.get(&session_id);
                    (
                        session.map(|s| s.model.clone()).unwrap_or_default(),
                        session.and_then(|s| s.mcp.clone()),
                    )
                };
                let out = out.clone();
                let waiting = waiting.clone();
                tokio::spawn(async move {
                    chunk(
                        &out,
                        &session_id,
                        "agent_thought_chunk",
                        "Mock Antigravity thinking.",
                    );
                    let has_result = prompt.contains("### tool result");
                    if prompt.contains("RUN_TOOL") && !has_result {
                        if let Some((url, token)) = mcp {
                            let asker = (&out, &waiting, session_id.as_str());
                            if let Err(err) = call_tool(&url, &token, &prompt, asker).await {
                                chunk(
                                    &out,
                                    &session_id,
                                    "agent_message_chunk",
                                    &format!("tool call failed: {err}"),
                                );
                            }
                            // Ferry should drop this and end the turn on its own.
                            chunk(
                                &out,
                                &session_id,
                                "agent_message_chunk",
                                "Waiting for the tool.",
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            send(
                                &out,
                                json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}),
                            );
                            return;
                        }
                    }
                    let reply_text = if has_result {
                        let result = prompt.rsplit("### tool result").next().unwrap_or("");
                        let line = result
                            .lines()
                            .rfind(|l| l.contains("ferry-acp-tool-ok"))
                            .unwrap_or("?");
                        format!("Tool said: {}", line.trim())
                    } else {
                        format!("Hello from {model} via ACP.")
                    };
                    for piece in reply_text.split_inclusive(' ') {
                        chunk(&out, &session_id, "agent_message_chunk", piece);
                    }
                    send(
                        &out,
                        json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn",
                        "usage": {"totalTokens": 30, "inputTokens": 25, "outputTokens": 5}}}),
                    );
                });
            }
            "session/close" => {
                sessions
                    .lock()
                    .unwrap()
                    .remove(params["sessionId"].as_str().unwrap_or(""));
                send(&out, reply(json!({})));
            }
            "session/cancel" => {}
            _ if !id.is_null() => send(
                &out,
                json!({"jsonrpc": "2.0", "id": id,
                                                    "error": {"code": -32601, "message": "method not found"}}),
            ),
            _ => {}
        }
    }
}
