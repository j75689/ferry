//! A fake OpenAI-compatible provider for trying Ferry without an API key.
//!
//!     cargo run -p ferry-proxy --example mock_provider -- 9999
//!
//! Then add a provider with base_url `http://127.0.0.1:9999/v1`. It echoes
//! the last user message; if that message contains `RUN_TOOL` it first calls
//! the client's shell tool (`Bash` for Claude Code, `shell` for Codex).

use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

async fn chat(Json(body): Json<Value>) -> axum::response::Response {
    if body["stream"] != true {
        return Json(json!({"choices": [{"message": {"role": "assistant", "content": "OK"}, "finish_reason": "stop"}]}))
            .into_response();
    }
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    // Clients append their own context to user turns, so look at all of them.
    let user_text: Vec<String> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| text_of(&m["content"]))
        .collect();
    let wants_tool = user_text.iter().any(|t| t.contains("RUN_TOOL"));
    let first_line = user_text
        .last()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .to_string();
    let last_user: String = first_line.chars().take(80).collect();
    let tool = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|t| {
            let name = t["function"]["name"].as_str()?;
            matches!(name, "Bash" | "shell" | "exec_command").then(|| name.to_string())
        });
    let mut chunks =
        vec![json!({"choices":[{"delta":{"reasoning_content":"Mock provider thinking."}}]})];
    let tool_output = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .map(|m| text_of(&m["content"]));
    match (tool, tool_output) {
        (Some(name), None) if wants_tool => {
            let args = match name.as_str() {
                "Bash" => {
                    json!({"command": "echo ferry-tool-ok", "description": "Ferry mock tool call"})
                }
                "exec_command" => json!({"cmd": "echo ferry-tool-ok"}),
                _ => json!({"command": ["echo", "ferry-tool-ok"]}),
            };
            chunks.push(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_mock1","type":"function",
                "function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]}));
        }
        (_, Some(output)) => {
            chunks.push(
                json!({"choices":[{"delta":{"content":format!("Tool said: {}", output.trim())}}]}),
            );
            chunks.push(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}));
        }
        _ => {
            chunks.push(
                json!({"choices":[{"delta":{"content":format!("Mock reply to: {last_user}")}}]}),
            );
            chunks.push(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}));
        }
    }
    chunks.push(json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10}}));
    let mut sse = String::new();
    for chunk in chunks {
        sse.push_str(&format!("data: {chunk}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], sse).into_response()
}

async fn models() -> Json<Value> {
    Json(json!({"object": "list", "data": [{"id": "mock-1", "object": "model"}]}))
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(9999);
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    eprintln!("mock provider on http://127.0.0.1:{port}/v1");
    axum::serve(listener, app).await.unwrap();
}
