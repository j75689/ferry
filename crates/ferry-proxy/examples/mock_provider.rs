//! A fake OpenAI-compatible provider for trying Ferry without an API key.
//!
//!     cargo run -p ferry-proxy --example mock_provider -- 9999
//!
//! Then add a provider with base_url `http://127.0.0.1:9999/v1`. It echoes
//! the last user message; if that message contains `RUN_TOOL` it first calls
//! the client's shell tool (`Bash` for Claude Code, `shell` for Codex).
//! Requests for the model `flaky` drop the connection halfway through the
//! first answer, to exercise stream resumption. `POST /v1/messages` behaves
//! the same way in Anthropic format, for `kind = "anthropic"` providers.
//! When offered Ferry's `web_search` function and asked to `SEARCH_WEB` (or
//! by Claude Code's WebSearch), it searches once and quotes the first
//! result; `GET /search` is a SearXNG-style backend with fixed results.

use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
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

fn flaky(messages: &[Value]) -> axum::response::Response {
    let resuming = messages
        .last()
        .is_some_and(|m| text_of(&m["content"]).contains("cut off"));
    let frame = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":delta,"finish_reason":finish}]})
        )
    };
    if resuming {
        let body = frame(
            json!({"content":"the quick brown fox jumps over the lazy dog."}),
            json!(null),
        ) + &frame(json!({}), json!("stop"))
            + "data: [DONE]\n\n";
        return ([("content-type", "text/event-stream")], body).into_response();
    }
    let head = frame(
        json!({"content":"Before the outage, the quick brown fo"}),
        json!(null),
    );
    let stream = futures_util::stream::iter([Ok::<_, std::io::Error>(head)]).chain(
        futures_util::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Err(std::io::Error::other("simulated outage"))
        }),
    );
    (
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn chat(Json(body): Json<Value>) -> axum::response::Response {
    if body["stream"] != true {
        return Json(json!({"choices": [{"message": {"role": "assistant", "content": "OK"}, "finish_reason": "stop"}]}))
            .into_response();
    }
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    if body["model"] == "flaky" {
        return flaky(&messages);
    }
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
    if let Some(reply) = web_search_turn(&body, &messages, &user_text) {
        chunks.extend(reply);
        chunks.push(json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10}}));
        let mut sse = String::new();
        for chunk in chunks {
            sse.push_str(&format!("data: {chunk}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
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

/// Search once through Ferry's `web_search`, then quote the first result.
fn web_search_turn(body: &Value, messages: &[Value], user_text: &[String]) -> Option<Vec<Value>> {
    let offered = body["tools"]
        .as_array()?
        .iter()
        .any(|t| t["function"]["name"] == "web_search");
    let query = user_text.iter().find_map(|t| {
        t.split_once("Perform a web search for the query:")
            .map(|(_, q)| q.trim().to_string())
            .or_else(|| {
                t.contains("SEARCH_WEB")
                    .then(|| "ferry rust gateway".to_string())
            })
    })?;
    let result = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .map(|m| text_of(&m["content"]));
    if !offered {
        // Claude Code's main turn: call its WebSearch tool, which sends the
        // search request with the hosted tool.
        let has_web_search = body["tools"]
            .as_array()?
            .iter()
            .any(|t| t["function"]["name"] == "WebSearch");
        if !has_web_search || result.is_some() {
            return None;
        }
        return Some(vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_ws1","type":"function",
            "function":{"name":"WebSearch","arguments":json!({"query": query}).to_string()}}]},
            "finish_reason":"tool_calls"}]}),
        ]);
    }
    Some(match result {
        None => vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_search1","type":"function",
            "function":{"name":"web_search","arguments":json!({"query": query}).to_string()}}]},
            "finish_reason":"tool_calls"}]}),
        ],
        Some(result) => {
            let first = result
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with("http"))
                .unwrap_or("nothing")
                .to_string();
            vec![
                json!({"choices":[{"delta":{"content":format!("Search said: {first}")}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ]
        }
    })
}

/// A SearXNG-style search backend: `GET /search?q=...&format=json`.
async fn search(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let query = params.get("q").cloned().unwrap_or_default();
    Json(json!({"results": [
        {"title": format!("{query} - first result"), "url": "https://example.com/first", "content": "The first mock result."},
        {"title": "Second", "url": "https://example.org/second", "content": "Another mock result."}
    ]}))
}

/// The same behaviour as `chat`, speaking the Anthropic Messages API.
async fn messages(Json(body): Json<Value>) -> axum::response::Response {
    let blocks = |m: &Value| -> Vec<Value> {
        match &m["content"] {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            Value::Array(items) => items.clone(),
            _ => Vec::new(),
        }
    };
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let user_text: Vec<String> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .flat_map(&blocks)
        .filter_map(|b| b["text"].as_str().map(str::to_string))
        .collect();
    let tool_output = messages
        .iter()
        .rev()
        .flat_map(&blocks)
        .find(|b| b["type"] == "tool_result")
        .map(|b| match &b["content"] {
            Value::String(text) => text.clone(),
            other => text_of(other),
        });
    let tool = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|t| {
            let name = t["name"].as_str()?;
            matches!(name, "Bash" | "shell" | "exec_command").then(|| name.to_string())
        });
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "msg_mock", "type": "message", "role": "assistant", "content": [],
               "model": body["model"], "usage": {"input_tokens": 100, "output_tokens": 0}}}),
    ];
    let stop = match (tool, tool_output) {
        (Some(name), None) if user_text.iter().any(|t| t.contains("RUN_TOOL")) => {
            let args = match name.as_str() {
                "Bash" => {
                    json!({"command": "echo ferry-tool-ok", "description": "Ferry mock tool call"})
                }
                "exec_command" => json!({"cmd": "echo ferry-tool-ok"}),
                _ => json!({"command": ["echo", "ferry-tool-ok"]}),
            };
            events.push(json!({"type": "content_block_start", "index": 0,
                               "content_block": {"type": "tool_use", "id": "toolu_mock1", "name": name, "input": {}}}));
            events.push(json!({"type": "content_block_delta", "index": 0,
                               "delta": {"type": "input_json_delta", "partial_json": args.to_string()}}));
            events.push(json!({"type": "content_block_stop", "index": 0}));
            "tool_use"
        }
        (_, output) => {
            let text = match output {
                Some(output) => format!("Tool said: {}", output.trim()),
                None => format!(
                    "Mock reply to: {}",
                    user_text
                        .last()
                        .map(|t| t.lines().next().unwrap_or(""))
                        .unwrap_or("")
                ),
            };
            events.push(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
            events.push(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}));
            events.push(json!({"type": "content_block_stop", "index": 0}));
            "end_turn"
        }
    };
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 10}}));
    events.push(json!({"type": "message_stop"}));
    let sse: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("")))
        .collect();
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
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
        .route("/search", get(search));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    eprintln!("mock provider on http://127.0.0.1:{port}/v1");
    axum::serve(listener, app).await.unwrap();
}
