//! Anthropic Messages ingress: request conversion to OpenAI Chat and
//! re-encoding neutral events as Anthropic SSE.

use serde_json::{json, Map, Value};

use crate::config::ProviderConfig;
use crate::stream::{new_id, Event, Finish, Usage};

/// Build an OpenAI Chat request for `model` from an Anthropic Messages body.
pub fn to_chat_request(body: &Value, model: &str, provider: &ProviderConfig) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = system_text(body.get("system")) {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => messages.push(assistant_message(message.get("content"), provider)),
            _ => user_messages(message.get("content"), &mut messages),
        }
    }

    let mut request = Map::new();
    request.insert("model".into(), json!(model));
    request.insert("messages".into(), Value::Array(messages));
    request.insert("stream".into(), json!(true));
    if provider.stream_usage {
        request.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(max) = body.get("max_tokens").and_then(Value::as_u64) {
        let capped = provider
            .max_tokens_cap
            .map_or(max, |cap| max.min(cap as u64));
        request.insert("max_tokens".into(), json!(capped));
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = body.get(key).filter(|v| v.is_number()) {
            request.insert(key.into(), value.clone());
        }
    }
    if let Some(stops) = body.get("stop_sequences").and_then(Value::as_array) {
        if !stops.is_empty() {
            request.insert(
                "stop".into(),
                Value::Array(stops.iter().take(4).cloned().collect()),
            );
        }
    }

    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| is_client_tool(tool))
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.get("name").cloned().unwrap_or(json!("")),
                    "description": tool.get("description").cloned().unwrap_or(json!("")),
                    "parameters": clean_schema(tool.get("input_schema")),
                }
            })
        })
        .collect();
    if !tools.is_empty() {
        request.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = body.get("tool_choice") {
            let kind = choice.get("type").and_then(Value::as_str).unwrap_or("auto");
            let mapped = match kind {
                "any" => json!("required"),
                "none" => json!("none"),
                "tool" => json!({"type": "function", "function": {"name": choice.get("name")}}),
                _ => json!("auto"),
            };
            request.insert("tool_choice".into(), mapped);
            if choice
                .get("disable_parallel_tool_use")
                .and_then(Value::as_bool)
                == Some(true)
            {
                request.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }

    if provider.send_reasoning_effort {
        if let Some(effort) = thinking_effort(body.get("thinking")) {
            request.insert("reasoning_effort".into(), json!(effort));
        }
    }
    Value::Object(request)
}

fn system_text(system: Option<&Value>) -> Option<String> {
    let text = match system? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// Server tools (`web_search_*` etc.) carry a versioned `type` and cannot be
/// executed by OpenAI-compatible providers.
fn is_client_tool(tool: &Value) -> bool {
    match tool.get("type").and_then(Value::as_str) {
        None | Some("custom") => tool.get("name").is_some(),
        Some(_) => false,
    }
}

/// Drop JSON Schema keywords that strict OpenAI-compatible servers reject.
pub fn clean_schema(schema: Option<&Value>) -> Value {
    fn walk(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, _)| k.as_str() != "$schema")
                    .map(|(k, v)| (k.clone(), walk(v)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(walk).collect()),
            other => other.clone(),
        }
    }
    match schema {
        Some(schema @ Value::Object(_)) => walk(schema),
        _ => json!({"type": "object", "properties": {}}),
    }
}

fn thinking_effort(thinking: Option<&Value>) -> Option<&'static str> {
    let thinking = thinking?;
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return None;
    }
    let budget = thinking
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(match budget {
        0..=4095 => "low",
        4096..=16383 => "medium",
        _ => "high",
    })
}

fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => Vec::new(),
    }
}

fn image_part(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => format!(
            "data:{};base64,{}",
            source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png"),
            source.get("data").and_then(Value::as_str)?
        ),
        Some("url") => source.get("url").and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    Some(json!({"type": "image_url", "image_url": {"url": url}}))
}

/// A user turn may hold tool results, which OpenAI wants as separate `tool`
/// messages placed before any new user text.
fn user_messages(content: Option<&Value>, out: &mut Vec<Value>) {
    let mut parts = Vec::new();
    for block in content_blocks(content) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(json!({"type": "text", "text": text}));
                }
            }
            Some("image") => parts.extend(image_part(&block)),
            Some("document") => {
                if let Some(text) = block.pointer("/source/data").and_then(Value::as_str) {
                    if block.pointer("/source/type").and_then(Value::as_str) == Some("text") {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                }
            }
            Some("tool_result") => {
                let (text, images) = tool_result_content(block.get("content"));
                let text = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    format!("Error: {text}")
                } else {
                    text
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
                    "content": text,
                }));
                parts.extend(images);
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return;
    }
    let only_text = parts
        .iter()
        .all(|p| p.get("type").and_then(Value::as_str) == Some("text"));
    let content = if only_text {
        json!(parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"))
    } else {
        Value::Array(parts)
    };
    out.push(json!({"role": "user", "content": content}));
}

fn tool_result_content(content: Option<&Value>) -> (String, Vec<Value>) {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in content_blocks(content) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => texts.extend(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
            Some("image") => {
                if let Some(part) = image_part(&block) {
                    texts.push("[image attached below]".into());
                    images.push(part);
                }
            }
            _ => {}
        }
    }
    (texts.join("\n"), images)
}

fn assistant_message(content: Option<&Value>, provider: &ProviderConfig) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for block in content_blocks(content) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => {
                reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or(""))
            }
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or(json!({}));
                tool_calls.push(json!({
                    "id": block.get("id").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or(json!("")),
                        "arguments": input.to_string(),
                    }
                }));
            }
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(text));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if provider.reasoning_history && !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    Value::Object(message)
}

/// Rough token estimate for `count_tokens` and `message_start` usage.
pub fn estimate_input_tokens(body: &Value) -> u64 {
    let mut chars = 0usize;
    for key in ["system", "messages", "tools"] {
        if let Some(value) = body.get(key) {
            chars += value.to_string().len();
        }
    }
    (chars / 4).max(1) as u64
}

#[derive(Clone, Copy, PartialEq)]
enum Block {
    Thinking,
    Text,
}

/// Turns neutral events into Anthropic `message_*` / `content_block_*` SSE.
pub struct AnthropicEncoder {
    model: String,
    input_estimate: u64,
    index: usize,
    open: Option<Block>,
    started: bool,
    finished: bool,
    usage: Usage,
}

pub type SseOut = (&'static str, Value);

impl AnthropicEncoder {
    pub fn new(model: &str, input_estimate: u64) -> Self {
        Self {
            model: model.to_string(),
            input_estimate,
            index: 0,
            open: None,
            started: false,
            finished: false,
            usage: Usage::default(),
        }
    }

    pub fn start(&mut self) -> Vec<SseOut> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": new_id("msg_"),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": self.input_estimate, "output_tokens": 0},
                }
            }),
        )]
    }

    fn close(&mut self, out: &mut Vec<SseOut>) {
        if let Some(block) = self.open.take() {
            if block == Block::Thinking {
                out.push((
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": self.index,
                           "delta": {"type": "signature_delta", "signature": "ferry"}}),
                ));
            }
            out.push((
                "content_block_stop",
                json!({"type": "content_block_stop", "index": self.index}),
            ));
            self.index += 1;
        }
    }

    fn open(&mut self, block: Block, out: &mut Vec<SseOut>) {
        if self.open == Some(block) {
            return;
        }
        self.close(out);
        let content_block = match block {
            Block::Thinking => json!({"type": "thinking", "thinking": "", "signature": ""}),
            Block::Text => json!({"type": "text", "text": ""}),
        };
        out.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": self.index, "content_block": content_block}),
        ));
        self.open = Some(block);
    }

    pub fn push(&mut self, event: Event) -> Vec<SseOut> {
        let mut out = self.start();
        if self.finished {
            return out;
        }
        match event {
            Event::Reasoning(text) => {
                self.open(Block::Thinking, &mut out);
                out.push((
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": self.index,
                           "delta": {"type": "thinking_delta", "thinking": text}}),
                ));
            }
            Event::Text(text) => {
                self.open(Block::Text, &mut out);
                out.push((
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": self.index,
                           "delta": {"type": "text_delta", "text": text}}),
                ));
            }
            Event::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.close(&mut out);
                // Keep the provider's id: it comes back as `tool_use_id` and
                // some providers validate it against their own format.
                out.push((
                    "content_block_start",
                    json!({"type": "content_block_start", "index": self.index,
                           "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
                ));
                out.push((
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": self.index,
                           "delta": {"type": "input_json_delta", "partial_json": normalize_arguments(&arguments)}}),
                ));
                out.push((
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": self.index}),
                ));
                self.index += 1;
            }
            Event::Usage(usage) => self.usage = usage,
            Event::Finish(finish) => {
                self.close(&mut out);
                let stop_reason = match finish {
                    Finish::Stop | Finish::ContentFilter => "end_turn",
                    Finish::Length => "max_tokens",
                    Finish::ToolCalls => "tool_use",
                };
                let input = if self.usage.input_tokens > 0 {
                    self.usage.input_tokens
                } else {
                    self.input_estimate
                };
                out.push((
                    "message_delta",
                    json!({"type": "message_delta",
                           "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                           "usage": {"input_tokens": input,
                                     "output_tokens": self.usage.output_tokens,
                                     "cache_read_input_tokens": self.usage.cached_tokens}}),
                ));
                out.push(("message_stop", json!({"type": "message_stop"})));
                self.finished = true;
            }
            Event::Error(message) => {
                out.push(("error", error_body("api_error", &message)));
                self.finished = true;
            }
        }
        out
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Tool arguments must be a JSON object for Claude Code.
fn normalize_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(map)) => Value::Object(map).to_string(),
        Ok(other) => json!({"value": other}).to_string(),
        Err(_) => json!({"_raw_arguments": arguments}).to_string(),
    }
}

pub fn error_body(error_type: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": error_type, "message": message}})
}

pub fn error_type_for_status(status: u16) -> &'static str {
    match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// Fold an encoded SSE sequence back into a non-streaming `message` object.
pub fn aggregate(frames: &[SseOut]) -> Result<Value, Value> {
    let mut message = json!({});
    let mut blocks: Vec<Value> = Vec::new();
    for (name, data) in frames {
        match *name {
            "message_start" => message = data["message"].clone(),
            "content_block_start" => blocks.push(data["content_block"].clone()),
            "content_block_delta" => {
                let Some(block) = blocks.last_mut() else {
                    continue;
                };
                let delta = &data["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => append(block, "text", &delta["text"]),
                    Some("thinking_delta") => append(block, "thinking", &delta["thinking"]),
                    Some("signature_delta") => block["signature"] = delta["signature"].clone(),
                    Some("input_json_delta") => {
                        block["input"] =
                            serde_json::from_str(delta["partial_json"].as_str().unwrap_or("{}"))
                                .unwrap_or(json!({}))
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                message["stop_reason"] = data["delta"]["stop_reason"].clone();
                message["usage"] = data["usage"].clone();
            }
            "error" => return Err(data.clone()),
            _ => {}
        }
    }
    message["content"] = Value::Array(blocks);
    Ok(message)
}

fn append(block: &mut Value, key: &str, text: &Value) {
    let existing = block[key].as_str().unwrap_or("").to_string();
    block[key] = json!(existing + text.as_str().unwrap_or(""));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;

    fn provider() -> ProviderConfig {
        PRESETS[0].to_provider()
    }

    #[test]
    fn converts_tool_round_trip_history() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "system": [{"type": "text", "text": "You are Claude Code."}],
            "tools": [
                {"name": "Read", "description": "read", "input_schema": {"$schema": "x", "type": "object", "properties": {"path": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "tool_choice": {"type": "any"},
            "messages": [
                {"role": "user", "content": "read a"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "need file", "signature": "s"},
                    {"type": "text", "text": "Reading."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "hello"}]},
                    {"type": "text", "text": "now summarize"}
                ]}
            ]
        });
        let chat = to_chat_request(&body, "meta/llama", &provider());
        assert_eq!(chat["model"], "meta/llama");
        assert_eq!(chat["max_tokens"], 16384);
        assert_eq!(chat["tool_choice"], "required");
        assert_eq!(chat["tools"].as_array().unwrap().len(), 1);
        assert!(chat["tools"][0]["function"]["parameters"]
            .get("$schema")
            .is_none());
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "You are Claude Code."})
        );
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert!(messages[2].get("reasoning_content").is_none());
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "hello"})
        );
        assert_eq!(
            messages[4],
            json!({"role": "user", "content": "now summarize"})
        );
    }

    #[test]
    fn encodes_and_aggregates_a_turn() {
        let mut encoder = AnthropicEncoder::new("claude-sonnet-4-5", 12);
        let mut frames = Vec::new();
        for event in [
            Event::Reasoning("think".into()),
            Event::Text("Hi".into()),
            Event::ToolCall {
                id: "call_9".into(),
                name: "Bash".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            },
            Event::Usage(Usage {
                input_tokens: 20,
                output_tokens: 7,
                ..Usage::default()
            }),
            Event::Finish(Finish::ToolCalls),
        ] {
            frames.extend(encoder.push(event));
        }
        let names: Vec<_> = frames.iter().map(|(n, _)| *n).collect();
        assert_eq!(names.first(), Some(&"message_start"));
        assert_eq!(names.last(), Some(&"message_stop"));
        let message = aggregate(&frames).unwrap();
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["content"][0]["thinking"], "think");
        assert_eq!(message["content"][1]["text"], "Hi");
        assert_eq!(message["content"][2]["input"], json!({"cmd": "ls"}));
        assert_eq!(message["usage"]["output_tokens"], 7);
    }
}
