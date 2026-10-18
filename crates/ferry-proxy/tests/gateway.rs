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
use futures_util::StreamExt;
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

/// A stream that stops early, optionally by aborting the connection.
fn partial(texts: &[&str], abort: bool) -> Response {
    let mut items: Vec<Result<String, std::io::Error>> = vec![Ok(format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"reasoning_content":"thinking"}}]})
    ))];
    for text in texts {
        items.push(Ok(format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":text},"finish_reason":null}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
        )));
    }
    // Abort only after the content was flushed, like a real mid-stream drop.
    let stream = futures_util::stream::iter(items).chain(futures_util::stream::once(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if abort {
            Err(std::io::Error::other("connection reset"))
        } else {
            Ok(String::new())
        }
    }));
    let body = axum::body::Body::from_stream(stream);
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn chat(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    fake.calls.fetch_add(1, Ordering::SeqCst);
    *fake.last_body.lock().unwrap() = body.clone();
    let resuming = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m["content"].as_str())
        .is_some_and(|c| c.contains("cut off"));
    match (body["model"].as_str(), resuming) {
        // Cut off: ends without finish_reason or [DONE].
        (Some("flaky"), false) => return partial(&["Hello wor"], false),
        // Connection dies mid-body.
        (Some("dropper"), false) => return partial(&["Line one.\nLine tw"], true),
        (Some("always_broken"), _) => return partial(&["again and again "], true),
        // Continuations restart the last words, as real models do.
        (Some("flaky"), true) => {
            return sse(&[
                json!({"choices":[{"delta":{"content":"Hello world, resumed."}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
            ])
        }
        (Some("dropper"), true) => {
            return sse(&[
                json!({"choices":[{"delta":{"content":"Line two.\nLine three."}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ])
        }
        _ => {}
    }
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
    if body["model"] == "interleaved" {
        return sse(&[
            json!({"choices":[{"delta":{"content":"Step 1."}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"now step 2"}}]}),
            json!({"choices":[{"delta":{"content":" Step 2."},"finish_reason":"stop"}]}),
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

fn text_of(frames: &[(String, Value)]) -> String {
    frames
        .iter()
        .filter_map(|(_, d)| {
            d["delta"]["text"].as_str().or_else(|| {
                d["delta"]
                    .as_str()
                    .filter(|_| d["type"] == "response.output_text.delta")
            })
        })
        .collect()
}

async fn ask_claude(gateway: &str) -> Vec<(String, Value)> {
    post_sse(
        &format!("{gateway}/v1/messages"),
        json!({"model": "claude-sonnet-4-5", "max_tokens": 100, "stream": true,
               "messages": [{"role": "user", "content": "say hello"}]}),
    )
    .await
}

#[tokio::test]
async fn resumes_a_stream_cut_off_without_finish() {
    let (upstream, fake) = start_fake().await;
    let (gateway, state) = start_gateway(&upstream, "fake/flaky").await;
    let frames = ask_claude(&gateway).await;
    assert_eq!(text_of(&frames), "Hello world, resumed.");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    // The continuation replayed what the client had already seen.
    let sent = fake.last_body.lock().unwrap().clone();
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages[messages.len() - 2]["content"], "Hello wor");
    // One text block, no duplicate thinking, usage summed over both attempts.
    let starts: Vec<&str> = frames
        .iter()
        .filter(|(n, _)| n == "content_block_start")
        .map(|(_, d)| d["content_block"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(starts, ["thinking", "text"]);
    let delta = frames.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(delta.1["usage"]["output_tokens"], 4);
    assert_eq!(
        state.recent_requests()[0].outcome,
        "ok (resumed on fake/flaky)"
    );
}

#[tokio::test]
async fn resumes_after_connection_reset() {
    let (upstream, _) = start_fake().await;
    let (gateway, _) = start_gateway(&upstream, "fake/dropper").await;
    let frames = ask_claude(&gateway).await;
    assert_eq!(text_of(&frames), "Line one.\nLine two.\nLine three.");
    assert_eq!(frames.last().unwrap().0, "message_stop");
}

#[tokio::test]
async fn gives_up_after_bounded_attempts() {
    let (upstream, fake) = start_fake().await;
    let (gateway, state) = start_gateway(&upstream, "fake/always_broken").await;
    let frames = ask_claude(&gateway).await;
    let (name, data) = frames.last().unwrap();
    assert_eq!(name, "error");
    assert!(
        data["error"]["message"]
            .as_str()
            .unwrap()
            .contains("provider stream failed"),
        "{data}"
    );
    // First attempt plus MAX_RECOVERIES continuations; the fallback
    // (fake/good) is only used for opening, which succeeded every time.
    assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
    assert!(state.recent_requests()[0].outcome.starts_with("error"));
}

#[tokio::test]
async fn codex_stream_is_resumed_too() {
    let (upstream, _) = start_fake().await;
    let (gateway, _) = start_gateway(&upstream, "fake/flaky").await;
    let frames = post_sse(
        &format!("{gateway}/v1/responses"),
        json!({"model": "fake/flaky", "stream": true,
               "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}),
    )
    .await;
    assert_eq!(text_of(&frames), "Hello world, resumed.");
    let completed = &frames.last().unwrap().1["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(
        completed["output"][1]["content"][0]["text"],
        "Hello world, resumed."
    );
}

#[tokio::test]
async fn reasoning_after_text_survives_when_nothing_broke() {
    let (upstream, _) = start_fake().await;
    let (gateway, _) = start_gateway(&upstream, "fake/interleaved").await;
    let frames = ask_claude(&gateway).await;
    let thinking: String = frames
        .iter()
        .filter_map(|(_, d)| d["delta"]["thinking"].as_str())
        .collect();
    assert_eq!(thinking, "now step 2");
    assert_eq!(text_of(&frames), "Step 1. Step 2.");
}

#[tokio::test]
async fn session_titles_are_answered_without_the_provider() {
    let (upstream, fake) = start_fake().await;
    let (gateway, state) = start_gateway(&upstream, "fake/good").await;
    let frames = post_sse(
        &format!("{gateway}/v1/messages"),
        json!({
            "model": "claude-haiku-4-5", "max_tokens": 128000, "stream": true, "tools": [],
            "output_config": {"format": {"type": "json_schema", "schema": {
                "type": "object", "properties": {"title": {"type": "string"}}}}},
            "system": [{"type": "text", "text": "You are naming a coding session so the user can pick it out."}],
            "messages": [{"role": "user", "content": "<session>\nrefactor the config loader\n</session>"}]
        }),
    )
    .await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let text: String = frames
        .iter()
        .filter_map(|(_, d)| d["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, r#"{"title":"Refactor the config loader"}"#);
    assert_eq!(frames.last().unwrap().0, "message_stop");
    let record = &state.recent_requests()[0];
    assert_eq!(record.outcome, "ok (local session title)");
    assert_eq!(record.routed_model, "local");
}

#[tokio::test]
async fn auto_mode_reviews_get_room_for_a_verdict() {
    let (upstream, fake) = start_fake().await;
    let (gateway, state) = start_gateway(&upstream, "fake/good").await;
    post_sse(
        &format!("{gateway}/v1/messages"),
        json!({
            "model": "claude-sonnet-5", "max_tokens": 64, "stream": true,
            "stop_sequences": ["</block>"], "thinking": {"type": "disabled"},
            "system": [{"type": "text", "text": "You are a security monitor.\nIf the action should be allowed:\n<block>no</block>"}],
            "messages": [{"role": "user", "content": "<transcript>\n{\"Bash\":\"touch /tmp/x\"}\n</transcript>"}]
        }),
    )
    .await;
    let sent = fake.last_body.lock().unwrap().clone();
    assert_eq!(sent["max_tokens"], ferry_proxy::review::MIN_REVIEW_TOKENS);
    assert!(sent.get("stop").is_none());
    assert_eq!(
        state.recent_requests()[0].api,
        "messages (auto-mode review)"
    );
}

/// An Anthropic-compatible upstream that answers every turn with a shell call.
async fn anthropic_upstream(seen: Arc<std::sync::Mutex<Value>>) -> String {
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                assert_eq!(headers["x-api-key"], "anthropic-key");
                *seen.lock().unwrap() = body;
                let events = [
                    json!({"type":"message_start","message":{"usage":{"input_tokens":30}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Listing."}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"shell","input":{}}}),
                    json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":[\"ls\"]}"}}),
                    json!({"type":"content_block_stop","index":1}),
                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
                    json!({"type":"message_stop"}),
                ];
                let body: String = events
                    .iter()
                    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                    .collect();
                ([("content-type", "text/event-stream")], body).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[tokio::test]
async fn codex_runs_on_an_anthropic_provider() {
    let seen = Arc::new(std::sync::Mutex::new(Value::Null));
    let upstream = anthropic_upstream(seen.clone()).await;
    let config = Config::from_toml(&format!(
        r#"
        [server]
        auth_token = "{TOKEN}"

        [models]
        default = "claude/claude-sonnet-4-5"

        [[providers]]
        id = "claude"
        kind = "anthropic"
        base_url = "{upstream}"
        api_key = "anthropic-key"
        "#
    ))
    .unwrap();
    let server = ferry_proxy::spawn(AppState::new(config, None), "127.0.0.1", 0)
        .await
        .unwrap();
    let frames = post_sse(
        &format!("{}/v1/responses", server.url()),
        json!({
            "model": "claude/claude-sonnet-4-5",
            "instructions": "You are Codex.",
            "stream": true,
            "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}],
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "ls"}]},
                {"type": "function_call", "call_id": "c0", "name": "shell", "arguments": "{\"command\":[\"pwd\"]}"},
                {"type": "function_call_output", "call_id": "c0", "output": "/repo"}
            ]
        }),
    )
    .await;
    // The upstream got a well-formed Anthropic request.
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["system"], "You are Codex.");
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["tools"][0]["name"], "shell");
    let roles: Vec<&str> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    // Codex got Responses items back.
    let done: Vec<&Value> = frames
        .iter()
        .filter(|(n, _)| n == "response.output_item.done")
        .map(|(_, d)| &d["item"])
        .collect();
    assert_eq!(done[0]["type"], "message");
    assert_eq!(done[0]["content"][0]["text"], "Listing.");
    assert_eq!(done[1]["type"], "function_call");
    assert_eq!(done[1]["call_id"], "toolu_9");
    assert_eq!(done[1]["arguments"], "{\"command\":[\"ls\"]}");
    let completed = &frames.last().unwrap().1["response"];
    assert_eq!(completed["usage"]["input_tokens"], 30);
    assert_eq!(completed["usage"]["output_tokens"], 7);
    server.stop().await;
}
