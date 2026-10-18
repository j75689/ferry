//! Antigravity through ACP: install from a registry, sign in, and serve
//! Claude Code and Codex requests, against the bundled mock ACP server.

use std::io::Write;
use std::path::PathBuf;

use axum::routing::get;
use axum::Router;
use ferry_proxy::stream::SseParser;
use ferry_proxy::{AppState, Config};
use serde_json::{json, Value};

const TOKEN: &str = "test-token";

fn mock_server_binary() -> PathBuf {
    // Test binaries live in target/<profile>/deps; examples one level up.
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().join("examples");
    let path = dir.join(format!("mock_acp_server{}", std::env::consts::EXE_SUFFIX));
    assert!(path.exists(), "build examples first: {}", path.display());
    path
}

fn zip_with(binary: &[u8], name: &str) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        // Stored: deflating a debug binary dominates the test's run time.
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        zip.start_file(name, options).unwrap();
        zip.write_all(binary).unwrap();
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

/// Serves `/registry.json` and `/agent-<version>.zip` for the given version.
async fn start_registry(version: &'static str) -> String {
    let binary = std::fs::read(mock_server_binary()).unwrap();
    let cmd = format!("agy_acp_server{}", std::env::consts::EXE_SUFFIX);
    let archive = zip_with(&binary, &cmd);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let platform = ferry_proxy::acp::install::platform_key();
    let entry = json!({
        "id": "antigravity-acp",
        "version": version,
        "distribution": {"binary": {platform: {"archive": format!("{base}/agent-{version}.zip"), "cmd": format!("./{cmd}")}}}
    });
    let app = Router::new()
        .route(
            "/registry.json",
            get(move || async move { axum::Json(entry) }),
        )
        .route(
            &format!("/agent-{version}.zip"),
            get(move || async move { archive }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("{base}/registry.json")
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

fn text(frames: &[(String, Value)]) -> String {
    frames
        .iter()
        .filter_map(|(name, d)| match name.as_str() {
            "content_block_delta" => d["delta"]["text"].as_str(),
            "response.output_text.delta" => d["delta"].as_str(),
            _ => None,
        })
        .collect()
}

// One test, because the registry URL and mock sign-in state are process-wide.
// Multi-threaded like the desktop app, so ordering races show up here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn install_sign_in_and_serve_both_clients() {
    let scratch = std::env::temp_dir().join(ferry_proxy::stream::new_id("ferry-acp-test-"));
    std::fs::create_dir_all(&scratch).unwrap();
    std::env::set_var("MOCK_ACP_STATE", scratch.join("signed-in"));
    std::env::set_var("FERRY_ACP_REGISTRY_URL", start_registry("1.0.0").await);

    let config = Config::from_toml(&format!(
        r#"
        [server]
        auth_token = "{TOKEN}"

        [models]
        default = "antigravity/gemini-mock-flash"

        [[providers]]
        id = "antigravity"
        kind = "antigravity"
        "#
    ))
    .unwrap();
    let state = AppState::with_acp_root(config, None, scratch.join("acp"));
    let acp = state.acp().clone();

    // Install.
    assert!(acp.installed().is_none());
    let installed = acp.install_latest(|_| {}).await.unwrap();
    assert_eq!(installed.version, "1.0.0");
    let status = acp.status(true).await;
    assert!(status.running, "{:?}", status.error);
    assert_eq!(status.auth_methods.len(), 2);

    // Sessions need sign-in first.
    let err = acp.models().await.unwrap_err();
    assert!(err.contains("sign in"), "{err}");
    acp.authenticate("oauth-personal").await.unwrap();
    let models = acp.models().await.unwrap();
    assert_eq!(models.len(), 2);

    let server = ferry_proxy::spawn(state.clone(), "127.0.0.1", 0)
        .await
        .unwrap();
    let gateway = server.url();

    // Claude Code: the agent calls Claude Code's own Bash tool through MCP.
    let tools = json!([{"name": "Bash", "description": "Run a command",
                        "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}]);
    let frames = post_sse(
        &format!("{gateway}/v1/messages"),
        json!({"model": "claude-sonnet-4-5", "max_tokens": 1000, "stream": true, "tools": tools,
               "messages": [{"role": "user", "content": "RUN_TOOL please"}]}),
    )
    .await;
    let tool = frames
        .iter()
        .find(|(n, d)| n == "content_block_start" && d["content_block"]["type"] == "tool_use")
        .map(|(_, d)| d["content_block"].clone())
        .expect("a tool_use block");
    assert_eq!(tool["name"], "Bash");
    let input = frames
        .iter()
        .find_map(|(_, d)| d["delta"]["partial_json"].as_str())
        .unwrap();
    assert!(input.contains("echo ferry-acp-tool-ok"), "{input}");
    assert_eq!(
        text(&frames),
        "",
        "text after the tool call must be dropped"
    );
    let thinking: String = frames
        .iter()
        .filter_map(|(_, d)| d["delta"]["thinking"].as_str())
        .collect();
    assert_eq!(thinking, "Mock Antigravity thinking.");
    let stop = frames.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(stop.1["delta"]["stop_reason"], "tool_use");

    // Next turn carries the tool result; the agent answers in text.
    let frames = post_sse(
        &format!("{gateway}/v1/messages"),
        json!({"model": "claude-sonnet-4-5", "max_tokens": 1000, "stream": true, "tools": tools,
        "messages": [
            {"role": "user", "content": "RUN_TOOL please"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": tool["id"], "name": "Bash",
                                               "input": {"command": "echo ferry-acp-tool-ok"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool["id"],
                                          "content": "ferry-acp-tool-ok"}]}
        ]}),
    )
    .await;
    assert_eq!(text(&frames), "Tool said: ferry-acp-tool-ok");
    let stop = frames.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(stop.1["usage"]["output_tokens"], 5);

    // Codex: same agent behind the Responses API, with an explicit model.
    let frames = post_sse(
        &format!("{gateway}/v1/responses"),
        json!({"model": "antigravity/gemini-mock-pro", "stream": true,
               "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}),
    )
    .await;
    assert_eq!(text(&frames), "Hello from gemini-mock-pro via ACP.");
    assert_eq!(frames.last().unwrap().0, "response.completed");

    // Unknown models are reported with the available ones.
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/messages"))
        .header("x-api-key", TOKEN)
        .json(&json!({"model": "antigravity/nope", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("gemini-mock-pro"),
        "{body}"
    );

    // Update: a newer registry version replaces the old install.
    std::env::set_var("FERRY_ACP_REGISTRY_URL", start_registry("1.1.0").await);
    assert_eq!(acp.latest().await.unwrap().version, "1.1.0");
    let installed = acp.install_latest(|_| {}).await.unwrap();
    assert_eq!(installed.version, "1.1.0");
    assert!(!scratch.join("acp").join("1.0.0").exists());
    assert!(acp.status(true).await.running);

    acp.stop().await;
    server.stop().await;
    let _ = std::fs::remove_dir_all(&scratch);
}
