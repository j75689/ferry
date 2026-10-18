//! Serve an OpenAI Chat-shaped request with an ACP agent acting as the model.
//!
//! Each request gets a fresh agent session. The conversation is rendered as
//! a transcript, the client's tools are offered through the loopback MCP
//! server, and the agent's own tools are refused. A tool call ends the turn
//! and goes back to the client; its result arrives with the next request.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::client::{Connection, RpcError, AUTH_REQUIRED};
use super::mcp::ToolCall;
use super::AcpManager;
use crate::stream::{new_id, Event, Finish, Usage};
use crate::upstream::{EventStream, UpstreamError};

const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// After the first tool call, wait this long for parallel calls before ending the turn.
const TOOL_BATCH_WINDOW: Duration = Duration::from_millis(1500);

const INSTRUCTIONS: &str = "You are the language model behind a coding assistant that runs on the user's machine.
The assistant's tools are available to you through the MCP server named \"ferry\".
- To act, call those tools. The assistant runs them and their results appear in the conversation on the next turn.
- Never use any other tool: no built-in file, edit, shell, search, browser, or web tools.
- After calling tools, end your turn immediately without further text.
- When no tool is needed, reply with the final answer only.";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
}

fn error(status: u16, message: impl Into<String>) -> UpstreamError {
    UpstreamError {
        status,
        message: message.into(),
    }
}

pub fn rpc_failure(err: &RpcError) -> UpstreamError {
    if err.code == AUTH_REQUIRED || err.message.to_ascii_lowercase().contains("auth") {
        return error(
            401,
            format!("antigravity: sign in to Google Antigravity in the Ferry app first ({err})"),
        );
    }
    error(502, format!("antigravity: {err}"))
}

/// Model choices advertised in a `session/new` response.
pub fn model_options(session: &Value) -> (Option<String>, Vec<ModelOption>) {
    let mut models = Vec::new();
    if let Some(options) = session["configOptions"].as_array() {
        for option in options.iter().filter(|o| o["category"] == "model") {
            let mut push = |item: &Value| {
                if let Some(id) = item["value"].as_str() {
                    models.push(ModelOption {
                        id: id.to_string(),
                        name: item["name"].as_str().unwrap_or(id).to_string(),
                    });
                }
            };
            for item in option["options"].as_array().into_iter().flatten() {
                match item.get("options").and_then(Value::as_array) {
                    Some(group) => group.iter().for_each(&mut push),
                    None => push(item),
                }
            }
            if !models.is_empty() {
                return (option["id"].as_str().map(str::to_string), models);
            }
        }
    }
    // Older agents: the unstable `models` session field.
    for item in session
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = item["modelId"].as_str() {
            models.push(ModelOption {
                id: id.to_string(),
                name: item["name"].as_str().unwrap_or(id).to_string(),
            });
        }
    }
    (None, models)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Render a chat request's messages as prompt content blocks.
pub fn render_prompt(request: &Value, system_inline: bool, images: bool) -> Vec<Value> {
    let mut text = String::from(INSTRUCTIONS);
    let mut blocks = Vec::new();
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    if system_inline {
        let system: Vec<String> = messages
            .iter()
            .filter(|m| m["role"] == "system")
            .map(|m| text_of(&m["content"]))
            .collect();
        if !system.is_empty() {
            text.push_str("\n\n<assistant_instructions>\n");
            text.push_str(&system.join("\n\n"));
            text.push_str("\n</assistant_instructions>");
        }
    }
    text.push_str("\n\n<conversation>");
    for message in messages.iter().filter(|m| m["role"] != "system") {
        match message["role"].as_str().unwrap_or("user") {
            "assistant" => {
                text.push_str("\n\n### assistant\n");
                text.push_str(&text_of(&message["content"]));
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    text.push_str(&format!(
                        "\n[called tool {} (id {}) with {}]",
                        call.pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        call["id"].as_str().unwrap_or(""),
                        call.pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}"),
                    ));
                }
            }
            "tool" => {
                text.push_str(&format!(
                    "\n\n### tool result (id {})\n{}",
                    message["tool_call_id"].as_str().unwrap_or(""),
                    text_of(&message["content"])
                ));
            }
            _ => {
                text.push_str("\n\n### user\n");
                match &message["content"] {
                    Value::Array(parts) => {
                        for part in parts {
                            match part["type"].as_str() {
                                Some("image_url") => {
                                    let url = part
                                        .pointer("/image_url/url")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    match url
                                        .strip_prefix("data:")
                                        .and_then(|r| r.split_once(";base64,"))
                                    {
                                        Some((mime, data)) if images => {
                                            text.push_str("[image attached]");
                                            blocks.push(json!({"type": "image", "mimeType": mime, "data": data}));
                                        }
                                        _ => text.push_str("[image omitted]"),
                                    }
                                }
                                _ => text.push_str(part["text"].as_str().unwrap_or("")),
                            }
                        }
                    }
                    other => text.push_str(&text_of(other)),
                }
            }
        }
    }
    text.push_str(
        "\n</conversation>\n\nContinue as the assistant, responding to the latest message.",
    );
    let mut prompt = vec![json!({"type": "text", "text": text})];
    prompt.extend(blocks);
    prompt
}

fn system_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "system")
        .map(|m| text_of(&m["content"]))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Ends the agent session when the client stream is dropped early.
struct SessionGuard {
    connection: Arc<Connection>,
    session_id: String,
    closed: bool,
}

impl SessionGuard {
    async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        finish_session(self.connection.clone(), self.session_id.clone()).await;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.closed {
            let connection = self.connection.clone();
            let session_id = self.session_id.clone();
            tokio::spawn(finish_session(connection, session_id));
        }
    }
}

async fn finish_session(connection: Arc<Connection>, session_id: String) {
    connection
        .notify("session/cancel", json!({"sessionId": session_id}))
        .await;
    connection.unsubscribe(&session_id);
    if connection
        .capability("/sessionCapabilities/close")
        .is_some()
    {
        let _ = connection
            .request(
                "session/close",
                json!({"sessionId": session_id}),
                Duration::from_secs(10),
            )
            .await;
    }
}

enum Incoming {
    Tool(ToolCall),
    Done(Result<Value, RpcError>),
    BatchClosed,
}

pub async fn open(
    manager: &AcpManager,
    model: &str,
    request: &Value,
) -> Result<EventStream, UpstreamError> {
    let connection = manager
        .connection()
        .await
        .map_err(|e| error(503, format!("antigravity: {e}")))?;
    let tools = request["tools"].as_array().cloned().unwrap_or_default();
    let (registration, mut tool_calls) = manager
        .mcp
        .register(&tools)
        .await
        .map_err(|e| error(500, format!("cannot start the MCP bridge: {e}")))?;
    let mut mcp_servers = Vec::new();
    if !tools.is_empty() {
        if connection.capability("/mcpCapabilities/http").is_none() {
            return Err(error(
                502,
                "antigravity: this ACP server version cannot reach HTTP MCP servers",
            ));
        }
        mcp_servers.push(json!({
            "type": "http",
            "name": "ferry",
            "url": registration.url,
            "headers": [{"name": "Authorization", "value": format!("Bearer {}", registration.token())}],
        }));
    }

    let system_supported = connection
        .capability("/sessionCapabilities/systemPrompt")
        .is_some();
    let mut params =
        json!({"cwd": manager.sandbox_dir().display().to_string(), "mcpServers": mcp_servers});
    let system = system_text(request);
    if system_supported && !system.is_empty() {
        params["systemPrompt"] =
            json!({"mode": "append", "content": [{"type": "text", "text": system}]});
    }
    let session = connection
        .request("session/new", params, SESSION_TIMEOUT)
        .await
        .map_err(|e| rpc_failure(&e))?;
    let session_id = session["sessionId"]
        .as_str()
        .ok_or_else(|| error(502, "antigravity: session/new returned no sessionId"))?
        .to_string();
    let mut guard = SessionGuard {
        connection: connection.clone(),
        session_id: session_id.clone(),
        closed: false,
    };

    select_model(&connection, &session, &session_id, model).await?;

    let images = connection.capability("/promptCapabilities/image").is_some();
    let prompt = render_prompt(request, !system_supported, images);
    // Updates are read straight from the session channel: the agent sends
    // them before its prompt response, and that order must survive.
    let tool_names = tools
        .iter()
        .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let mut updates = connection.subscribe(&session_id, tool_names);
    let (tx, mut rx) = mpsc::unbounded_channel::<Incoming>();
    tokio::spawn({
        let tx = tx.clone();
        async move {
            while let Some(call) = tool_calls.recv().await {
                if tx.send(Incoming::Tool(call)).is_err() {
                    break;
                }
            }
        }
    });
    tokio::spawn({
        let connection = connection.clone();
        let tx = tx.clone();
        let session_id = session_id.clone();
        async move {
            let result = connection
                .request(
                    "session/prompt",
                    json!({"sessionId": session_id, "prompt": prompt}),
                    PROMPT_TIMEOUT,
                )
                .await;
            let _ = tx.send(Incoming::Done(result));
        }
    });

    let stream = async_stream::stream! {
        let _registration = registration;
        let mut calls: Vec<ToolCall> = Vec::new();
        loop {
            let incoming = tokio::select! {
                biased;
                Some(update) = updates.recv() => {
                    // Text after a tool call would contradict "end your turn".
                    if calls.is_empty() {
                        if let Some(event) = update_event(&update) {
                            yield event;
                        }
                    }
                    continue;
                }
                incoming = rx.recv() => incoming,
            };
            let Some(incoming) = incoming else { break };
            if matches!(incoming, Incoming::Done(_)) {
                // Everything the agent streamed before answering is queued already.
                while let Ok(update) = updates.try_recv() {
                    if calls.is_empty() {
                        if let Some(event) = update_event(&update) {
                            yield event;
                        }
                    }
                }
            }
            match incoming {
                Incoming::Tool(call) => {
                    if calls.is_empty() {
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(TOOL_BATCH_WINDOW).await;
                            let _ = tx.send(Incoming::BatchClosed);
                        });
                    }
                    calls.push(call);
                }
                Incoming::BatchClosed => break,
                Incoming::Done(Ok(response)) => {
                    if calls.is_empty() {
                        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
                            yield Event::Usage(acp_usage(usage));
                        }
                        let stop = response["stopReason"].as_str().unwrap_or("end_turn");
                        match stop {
                            "cancelled" => yield Event::Error("antigravity: the agent cancelled the turn".into()),
                            "refusal" => yield Event::Finish(Finish::ContentFilter),
                            "max_tokens" => yield Event::Finish(Finish::Length),
                            _ => yield Event::Finish(Finish::Stop),
                        }
                        guard.close().await;
                        return;
                    }
                    break;
                }
                Incoming::Done(Err(err)) => {
                    if calls.is_empty() {
                        yield Event::Error(rpc_failure(&err).message);
                        guard.close().await;
                        return;
                    }
                    break;
                }
            }
        }
        for call in calls {
            yield Event::ToolCall { id: new_id("call_"), name: call.name, arguments: call.arguments.to_string() };
        }
        yield Event::Finish(Finish::ToolCalls);
        guard.close().await;
    };
    Ok(Box::pin(stream))
}

async fn select_model(
    connection: &Connection,
    session: &Value,
    session_id: &str,
    model: &str,
) -> Result<(), UpstreamError> {
    let (config_id, models) = model_options(session);
    if model.is_empty() || model == "default" || models.is_empty() {
        return Ok(());
    }
    let Some(choice) = models
        .iter()
        .find(|m| m.id == model || m.name.eq_ignore_ascii_case(model))
    else {
        let known: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        return Err(error(
            400,
            format!(
                "antigravity: unknown model {model:?}; available: {}",
                known.join(", ")
            ),
        ));
    };
    let (method, params) = match config_id {
        Some(config_id) => (
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": config_id, "value": choice.id}),
        ),
        None => (
            "session/set_model",
            json!({"sessionId": session_id, "modelId": choice.id}),
        ),
    };
    connection
        .request(method, params, SESSION_TIMEOUT)
        .await
        .map(|_| ())
        .map_err(|e| rpc_failure(&e))
}

fn update_event(update: &Value) -> Option<Event> {
    let kind = update
        .pointer("/update/sessionUpdate")
        .and_then(Value::as_str)?;
    let text = update
        .pointer("/update/content/text")
        .and_then(Value::as_str)?;
    if text.is_empty() {
        return None;
    }
    match kind {
        "agent_message_chunk" => Some(Event::Text(text.to_string())),
        "agent_thought_chunk" => Some(Event::Reasoning(text.to_string())),
        _ => None,
    }
}

fn acp_usage(usage: &Value) -> Usage {
    let num = |key: &str| usage[key].as_u64().unwrap_or(0);
    Usage {
        input_tokens: num("inputTokens"),
        output_tokens: num("outputTokens"),
        cached_tokens: num("cachedReadTokens"),
        reasoning_tokens: num("thoughtTokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_grouped_and_flat_model_options() {
        let session = json!({"configOptions": [
            {"id": "mode", "category": "mode", "type": "select", "options": [{"value": "default", "name": "Default"}]},
            {"id": "model", "category": "model", "type": "select", "currentValue": "a", "options": [
                {"group": "g", "name": "Gemini", "options": [{"value": "gemini-pro", "name": "Gemini Pro"}]},
                {"value": "flash", "name": "Flash"}
            ]}
        ]});
        let (id, models) = model_options(&session);
        assert_eq!(id.as_deref(), Some("model"));
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gemini-pro", "flash"]);
        let legacy = json!({"models": {"availableModels": [{"modelId": "m1", "name": "M1"}]}});
        assert_eq!(
            model_options(&legacy),
            (
                None,
                vec![ModelOption {
                    id: "m1".into(),
                    name: "M1".into()
                }]
            )
        );
    }

    #[test]
    fn renders_tool_history_as_transcript() {
        let request = json!({"messages": [
            {"role": "system", "content": "Be terse."},
            {"role": "user", "content": [{"type": "text", "text": "list files"},
                                          {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "a.txt"}
        ]});
        let prompt = render_prompt(&request, true, true);
        let text = prompt[0]["text"].as_str().unwrap();
        assert!(text.contains("<assistant_instructions>\nBe terse."));
        assert!(text.contains("[called tool Bash (id c1) with {\"command\":\"ls\"}]"));
        assert!(text.contains("### tool result (id c1)\na.txt"));
        assert_eq!(
            prompt[1],
            json!({"type": "image", "mimeType": "image/png", "data": "AAAA"})
        );
        let without = render_prompt(&request, false, false);
        assert!(!without[0]["text"].as_str().unwrap().contains("Be terse."));
        assert_eq!(without.len(), 1);
    }
}
