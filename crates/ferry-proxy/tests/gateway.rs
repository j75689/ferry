//! End-to-end: real HTTP between a client, the gateway, and a fake provider.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ferry_proxy::stream::SseParser;
use ferry_proxy::{AppState, Config};
use serde_json::{json, Value};

const TOKEN: &str = "test-token";

#[derive(Clone, Default)]
struct Fake {
    calls: Arc<AtomicUsize>,
    last_body: Arc<std::sync::Mutex<Value>>,
}

fn sse(chunks: &[Value]) -> Response {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn chat(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    fake.calls.fetch_add(1, Ordering::SeqCst);
    *fake.last_body.lock().unwrap() = body.clone();
    if body["model"] == "broken" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "model broken"}})),
        )
            .into_response();
    }
    let wants_tool = body["tools"].as_array().is_some_and(|t| !t.is_empty())
        && body["messages"].as_array().unwrap().last().unwrap()["role"] == "user";
    if wants_tool {
        let name = body["tools"][0]["function"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        return sse(&[
            json!({"choices":[{"delta":{"reasoning_content":"I should call a tool."}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":name,"arguments":"{\"command\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":9}}),
        ]);
    }
    sse(&[
        json!({"choices":[{"delta":{"content":"Hello"}}]}),
        json!({"choices":[{"delta":{"content":" world"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}),
    ])
}

async fn start_fake() -> (String, Fake) {
    let fake = Fake::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

async fn start_gateway(upstream: &str, default_model: &str) -> (String, AppState) {
    let config = Config::from_toml(&format!(
        r#"
        [server]
        auth_token = "{TOKEN}"
        max_retries = 0

        [models]
        default = "{default_model}"
        fallbacks = ["fake/good"]

        [[providers]]
        id = "fake"
        base_url = "{upstream}"
        api_key = "upstream-key"
        "#
    ))
    .unwrap();
    let state = AppState::new(config, None);
    let server = ferry_proxy::spawn(state.clone(), "127.0.0.1", 0)
        .await
        .unwrap();
    let url = server.url();
    std::mem::forget(server);
    (url, state)
}

async fn post_sse(url: &str, body: Value) -> Vec<(String, Value)> {
    let response = reqwest::Client::new()
        .post(url)
        .header("x-api-key", TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
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

#[tokio::test]
async fn claude_code_tool_turn_streams_anthropic_events() {
    let (upstream, fake) = start_fake().await;
    let (gateway, state) = start_gateway(&upstream, "fake/good").await;
    let frames = post_sse(
        &format!("{gateway}/v1/messages?beta=true"),
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "stream": true,
            "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "list files"}]
        }),
    )
    .await;
    let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(frames[1].1["content_block"]["type"], "thinking");
    assert_eq!(frames[5].1["content_block"]["name"], "Bash");
    assert_eq!(frames[5].1["content_block"]["id"], "call_abc");
    assert_eq!(frames[6].1["delta"]["partial_json"], "{\"command\":\"ls\"}");
    assert_eq!(frames[8].1["delta"]["stop_reason"], "tool_use");
    assert_eq!(frames[8].1["usage"]["output_tokens"], 9);

    let sent = fake.last_body.lock().unwrap().clone();
    assert_eq!(sent["model"], "good");
    assert_eq!(sent["stream"], true);

    let log = state.recent_requests();
    assert_eq!(log[0].routed_model, "fake/good");
    assert_eq!(log[0].outcome, "ok");
    assert_eq!(log[0].input_tokens, 42);
}

#[tokio::test]
async fn non_streaming_messages_are_aggregated() {
    let (upstream, _) = start_fake().await;
    let (gateway, _) = start_gateway(&upstream, "fake/good").await;
    let message: Value = reqwest::Client::new()
        .post(format!("{gateway}/v1/messages"))
        .bearer_auth(TOKEN)
        .json(&json!({"model": "claude-haiku-4-5", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(message["type"], "message");
    assert_eq!(message["content"][0]["text"], "Hello world");
    assert_eq!(message["stop_reason"], "end_turn");
    assert_eq!(message["model"], "claude-haiku-4-5");
}

#[tokio::test]
async fn falls_back_when_primary_rejects() {
    let (upstream, fake) = start_fake().await;
    let (gateway, state) = start_gateway(&upstream, "fake/broken").await;
    let frames = post_sse(
        &format!("{gateway}/v1/messages"),
        json!({"model": "claude-opus-4-1", "max_tokens": 10, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert!(frames.iter().any(|(_, d)| d["delta"]["text"] == "Hello"));
    assert_eq!(state.recent_requests()[0].routed_model, "fake/good");
}

#[tokio::test]
async fn codex_turn_streams_responses_events() {
    let (upstream, _) = start_fake().await;
    let (gateway, _) = start_gateway(&upstream, "fake/good").await;
    let frames = post_sse(
        &format!("{gateway}/v1/responses"),
        json!({
            "model": "fake/good",
            "instructions": "You are Codex.",
            "stream": true,
            "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}],
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "ls"}]}]
        }),
    )
    .await;
    let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.first(), Some(&"response.created"));
    assert_eq!(names.last(), Some(&"response.completed"));
    let done: Vec<&Value> = frames
        .iter()
        .filter(|(n, _)| n == "response.output_item.done")
        .map(|(_, d)| &d["item"])
        .collect();
    assert_eq!(done[0]["type"], "reasoning");
    assert_eq!(done[1]["type"], "function_call");
    assert_eq!(done[1]["call_id"], "call_abc");
    assert_eq!(done[1]["arguments"], "{\"command\":\"ls\"}");
    let completed = &frames.last().unwrap().1["response"];
    assert_eq!(completed["usage"]["input_tokens"], 42);
}

#[tokio::test]
async fn rejects_missing_token_and_lists_models() {
    let (upstream, _) = start_fake().await;
    let (gateway, _) = start_gateway(&upstream, "fake/good").await;
    let client = reqwest::Client::new();
    let browser = client
        .post(format!("{gateway}/v1/messages"))
        .header("x-api-key", TOKEN)
        .header("origin", "https://evil.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(browser.status(), 403);
    let denied = client
        .get(format!("{gateway}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let health = client
        .get(format!("{gateway}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    let models: Value = client
        .get(format!("{gateway}/v1/models"))
        .header("x-api-key", TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "fake/good");
}
