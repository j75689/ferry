//! Hosted web search: the gateway runs the search loop between a fake model
//! and a fake SearXNG instance.

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferry_proxy::stream::SseParser;
use ferry_proxy::{AppState, Config};
use serde_json::{json, Value};

const TOKEN: &str = "test-token";

#[derive(Clone, Default)]
struct Fake {
    chats: Arc<Mutex<Vec<Value>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], body).into_response()
}

/// Searches once when it can, then answers from the tool result.
async fn chat(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    fake.chats.lock().unwrap().push(body.clone());
    let can_search = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["function"]["name"] == "web_search");
    let result = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .map(str::to_string);
    let usage = json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3}});
    match (can_search, result) {
        (true, None) => sse(&[
            json!({"choices": [{"delta": {"content": "Let me look."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_s1", "type": "function",
                "function": {"name": "web_search", "arguments": "{\"query\":\"tokio select\"}"}}]},
                "finish_reason": "tool_calls"}]}),
            usage,
        ]),
        (_, Some(result)) => {
            let first = result
                .lines()
                .find(|l| l.trim_start().starts_with("http"))
                .unwrap_or(result.lines().next().unwrap_or(""))
                .trim()
                .to_string();
            sse(&[
                json!({"choices": [{"delta": {"content": format!("Found: {first}")}, "finish_reason": "stop"}]}),
                usage,
            ])
        }
        (false, None) => sse(&[
            json!({"choices": [{"delta": {"content": "No search offered."}, "finish_reason": "stop"}]}),
            usage,
        ]),
    }
}

async fn searxng(
    State(fake): State<Fake>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(params.get("format").map(String::as_str), Some("json"));
    fake.queries
        .lock()
        .unwrap()
        .push(params.get("q").cloned().unwrap_or_default());
    Json(json!({"results": [
        {"title": "Spam", "url": "https://spam.example/tokio", "content": "ads"},
        {"title": "Select | Tokio", "url": "https://tokio.rs/tokio/tutorial/select", "content": "Waits on multiple branches."},
        {"title": "select in tokio", "url": "https://docs.rs/tokio/latest/tokio/macro.select.html", "content": "Macro docs"}
    ]}))
}

async fn start(enabled: bool) -> (String, Fake, AppState) {
    let fake = Fake::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/search", get(searxng))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config::from_toml(&format!(
        r#"
        [server]
        auth_token = "{TOKEN}"
        max_retries = 0

        [models]
        default = "fake/model"

        [web_search]
        enabled = {enabled}
        backend = "searxng"
        url = "{base}"

        [[providers]]
        id = "fake"
        base_url = "{base}/v1"
        "#
    ))
    .unwrap();
    let state = AppState::new(config, None);
    let server = ferry_proxy::spawn(state.clone(), "127.0.0.1", 0)
        .await
        .unwrap();
    let url = server.url();
    std::mem::forget(server);
    (url, fake, state)
}

async fn post_sse(url: &str, body: Value) -> Vec<(String, Value)> {
    let response = reqwest::Client::new()
        .post(url)
        .header("x-api-key", TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.bytes().await.unwrap();
    let mut parser = SseParser::default();
    let mut frames = parser.push(&bytes);
    frames.extend(parser.finish());
    frames
        .into_iter()
        .map(|f| {
            (
                f.event.unwrap_or_default(),
                serde_json::from_str(&f.data).unwrap(),
            )
        })
        .collect()
}

/// Claude Code's WebSearch tool sends a request like this one.
fn claude_search(max_uses: u32) -> Value {
    json!({
        "model": "claude-sonnet-4-5", "max_tokens": 32000, "stream": true,
        "system": [{"type": "text", "text": "You run a web search for another Claude model."}],
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": max_uses,
                   "blocked_domains": ["spam.example"]}],
        "tool_choice": {"type": "auto"},
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Perform a web search for the query: tokio select"}]}]
    })
}

#[tokio::test]
async fn claude_code_web_search_is_run_by_ferry() {
    let (gateway, fake, _) = start(true).await;
    let frames = post_sse(&format!("{gateway}/v1/messages"), claude_search(8)).await;

    let starts: Vec<&Value> = frames
        .iter()
        .filter(|(n, _)| n == "content_block_start")
        .map(|(_, d)| &d["content_block"])
        .collect();
    let kinds: Vec<&str> = starts.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["text", "server_tool_use", "web_search_tool_result", "text"]
    );
    let use_block = starts[1];
    assert!(use_block["id"].as_str().unwrap().starts_with("srvtoolu_"));
    let input = frames
        .iter()
        .find_map(|(_, d)| d["delta"]["partial_json"].as_str())
        .unwrap();
    assert_eq!(input, r#"{"query":"tokio select"}"#);
    let results = starts[2]["content"].as_array().unwrap();
    assert_eq!(starts[2]["tool_use_id"], use_block["id"]);
    assert_eq!(results.len(), 2, "blocked domains are dropped");
    assert_eq!(results[0]["type"], "web_search_result");
    assert_eq!(results[0]["url"], "https://tokio.rs/tokio/tutorial/select");

    let text: String = frames
        .iter()
        .filter_map(|(_, d)| d["delta"]["text"].as_str())
        .collect();
    assert_eq!(
        text,
        "Let me look.Found: https://tokio.rs/tokio/tutorial/select"
    );
    let delta = &frames.iter().find(|(n, _)| n == "message_delta").unwrap().1;
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(delta["usage"]["server_tool_use"]["web_search_requests"], 1);
    assert_eq!(
        delta["usage"]["output_tokens"], 6,
        "usage covers both rounds"
    );

    assert_eq!(*fake.queries.lock().unwrap(), ["tokio select"]);
    let chats = fake.chats.lock().unwrap();
    assert_eq!(chats.len(), 2);
    let followup = chats[1]["messages"].as_array().unwrap();
    let assistant = &followup[followup.len() - 2];
    assert_eq!(assistant["content"], "Let me look.");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "web_search");
    let tool = followup.last().unwrap();
    assert_eq!(tool["tool_call_id"], "call_s1");
    assert!(tool["content"]
        .as_str()
        .unwrap()
        .contains("Waits on multiple branches."));
}

#[tokio::test]
async fn search_limits_and_switch_are_respected() {
    let (gateway, fake, _) = start(true).await;
    let frames = post_sse(&format!("{gateway}/v1/messages"), claude_search(0)).await;
    let result = frames
        .iter()
        .find(|(n, d)| {
            n == "content_block_start" && d["content_block"]["type"] == "web_search_tool_result"
        })
        .map(|(_, d)| d["content_block"]["content"].clone())
        .unwrap();
    assert_eq!(result["type"], "web_search_tool_result_error");
    assert_eq!(result["error_code"], "max_uses_exceeded");
    assert!(fake.queries.lock().unwrap().is_empty());

    let (gateway, fake, _) = start(false).await;
    let frames = post_sse(&format!("{gateway}/v1/messages"), claude_search(8)).await;
    let text: String = frames
        .iter()
        .filter_map(|(_, d)| d["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "No search offered.");
    assert!(fake.queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn codex_web_search_is_run_by_ferry() {
    let (gateway, fake, _) = start(true).await;
    let frames = post_sse(
        &format!("{gateway}/v1/responses"),
        json!({"model": "fake/model", "stream": true,
               "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object"}},
                         {"type": "web_search", "external_web_access": true}],
               "input": [{"type": "message", "role": "user",
                          "content": [{"type": "input_text", "text": "how does tokio select work?"}]}]}),
    )
    .await;
    let done: Vec<&Value> = frames
        .iter()
        .filter(|(n, _)| n == "response.output_item.done")
        .map(|(_, d)| &d["item"])
        .collect();
    let kinds: Vec<&str> = done.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["message", "web_search_call", "message"]);
    let call = done[1];
    assert_eq!(call["status"], "completed");
    assert_eq!(call["action"]["query"], "tokio select");
    assert_eq!(call["action"]["sources"].as_array().unwrap().len(), 3);
    assert!(frames
        .iter()
        .any(|(n, _)| n == "response.web_search_call.completed"));
    assert_eq!(
        done[2]["content"][0]["text"],
        "Found: https://spam.example/tokio"
    );
    let completed = &frames.last().unwrap().1;
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 20);

    // Codex's own tools stay; the hosted one becomes Ferry's function.
    let chats = fake.chats.lock().unwrap();
    let names: Vec<&str> = chats[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["exec_command", "web_search"]);
}

#[tokio::test]
async fn earlier_searches_are_kept_as_text() {
    let (gateway, fake, _) = start(true).await;
    post_sse(
        &format!("{gateway}/v1/responses"),
        json!({"model": "fake/model", "stream": true,
               "input": [
                   {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "search"}]},
                   {"type": "web_search_call", "id": "ws_1", "status": "completed",
                    "action": {"type": "search", "query": "tokio select"}},
                   {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "It waits."}]},
                   {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]}
               ]}),
    )
    .await;
    let chats = fake.chats.lock().unwrap();
    let assistant = &chats[0]["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"],
        "[Searched the web for: tokio select]\nIt waits."
    );
}
