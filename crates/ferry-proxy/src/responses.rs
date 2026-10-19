//! OpenAI Responses ingress (Codex): request conversion to OpenAI Chat and
//! re-encoding neutral events as Responses SSE.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::anthropic::clean_schema;
use crate::config::ProviderConfig;
use crate::stream::{new_id, Event, Finish, Usage};

const REASONING_PREFIX: &str = "ferry1:";

pub struct ChatConversion {
    pub request: Value,
    /// Freeform (`custom`) tools exposed to the provider as one-string functions.
    pub custom_tools: HashSet<String>,
    /// Functions from `namespace` tools: provider name -> (namespace, name).
    pub namespaced: HashMap<String, (String, String)>,
}

/// Functions inside `namespace` tools (Codex groups its sub-agent tools
/// this way), keyed by the flat name the provider sees: the function's own
/// name, or `namespace__name` when that is taken.
fn namespaced_functions(body: &Value) -> HashMap<String, (String, String)> {
    let tools: Vec<&Value> = body["tools"].as_array().into_iter().flatten().collect();
    let mut taken: HashMap<&str, usize> = HashMap::new();
    for tool in &tools {
        if tool["type"] == "namespace" {
            for inner in tool["tools"].as_array().into_iter().flatten() {
                if let Some(name) = inner["name"].as_str() {
                    *taken.entry(name).or_default() += 1;
                }
            }
        } else if let Some(name) = tool["name"].as_str() {
            *taken.entry(name).or_insert(0) += 2;
        }
    }
    let mut out = HashMap::new();
    for tool in tools.iter().filter(|t| t["type"] == "namespace") {
        let Some(namespace) = tool["name"].as_str() else {
            continue;
        };
        for inner in tool["tools"].as_array().into_iter().flatten() {
            if inner["type"] != "function" {
                continue;
            }
            let Some(name) = inner["name"].as_str() else {
                continue;
            };
            let flat = if taken.get(name) == Some(&1) {
                name.to_string()
            } else {
                format!("{namespace}__{name}")
            };
            out.insert(flat, (namespace.to_string(), name.to_string()));
        }
    }
    out
}

#[derive(Default)]
struct PendingAssistant {
    text: String,
    reasoning: String,
    tool_calls: Vec<Value>,
}

impl PendingAssistant {
    fn flush(&mut self, provider: &ProviderConfig, out: &mut Vec<Value>) {
        let pending = std::mem::take(self);
        if pending.text.is_empty() && pending.tool_calls.is_empty() {
            // Reasoning alone is carried into the next assistant turn.
            self.reasoning = pending.reasoning;
            return;
        }
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), json!(pending.text));
        if !pending.tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(pending.tool_calls));
        }
        if provider.reasoning_history && !pending.reasoning.is_empty() {
            message.insert("reasoning_content".into(), json!(pending.reasoning));
        }
        out.push(Value::Object(message));
    }
}

pub fn to_chat_request(body: &Value, model: &str, provider: &ProviderConfig) -> ChatConversion {
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    let namespaced = namespaced_functions(body);
    let mut assistant = PendingAssistant::default();
    match body.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                input_item(item, provider, &namespaced, &mut assistant, &mut messages);
            }
        }
        _ => {}
    }
    assistant.flush(provider, &mut messages);

    let mut custom_tools = HashSet::new();
    let mut tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| convert_tool(tool, &mut custom_tools))
        .collect();
    for tool in body["tools"].as_array().into_iter().flatten() {
        for inner in tool["tools"].as_array().into_iter().flatten() {
            let (Some(namespace), Some(name)) = (tool["name"].as_str(), inner["name"].as_str())
            else {
                continue;
            };
            let flat = namespaced
                .iter()
                .find(|(_, (ns, n))| ns == namespace && n == name)
                .map(|(flat, _)| flat.clone());
            if let (Some(flat), Some(mut function)) = (flat, convert_tool(inner, &mut custom_tools))
            {
                function["function"]["name"] = json!(flat);
                tools.push(function);
            }
        }
    }

    let mut request = Map::new();
    request.insert("model".into(), json!(model));
    request.insert("messages".into(), Value::Array(messages));
    request.insert("stream".into(), json!(true));
    if provider.stream_usage {
        request.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !tools.is_empty() {
        request.insert("tools".into(), Value::Array(tools));
        match body.get("tool_choice") {
            Some(Value::String(choice))
                if ["auto", "required", "none"].contains(&choice.as_str()) =>
            {
                request.insert("tool_choice".into(), json!(choice));
            }
            Some(choice @ Value::Object(_)) => {
                if let Some(name) = choice.get("name") {
                    request.insert(
                        "tool_choice".into(),
                        json!({"type": "function", "function": {"name": name}}),
                    );
                }
            }
            _ => {}
        }
        if let Some(parallel) = body.get("parallel_tool_calls").and_then(Value::as_bool) {
            request.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }
    if let Some(max) = body.get("max_output_tokens").and_then(Value::as_u64) {
        let capped = provider
            .max_tokens_cap
            .map_or(max, |cap| max.min(cap as u64));
        request.insert("max_tokens".into(), json!(capped));
    } else if let Some(cap) = provider.max_tokens_cap {
        request.insert("max_tokens".into(), json!(cap));
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = body.get(key).filter(|v| v.is_number()) {
            request.insert(key.into(), value.clone());
        }
    }
    if provider.send_reasoning_effort {
        if let Some(effort) = body.pointer("/reasoning/effort").and_then(Value::as_str) {
            let effort = if effort == "minimal" { "low" } else { effort };
            if effort != "none" {
                request.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }
    ChatConversion {
        request: Value::Object(request),
        custom_tools,
        namespaced,
    }
}

fn convert_tool(tool: &Value, custom_tools: &mut HashSet<String>) -> Option<Value> {
    let name = tool.get("name").and_then(Value::as_str)?;
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    match tool.get("type").and_then(Value::as_str) {
        Some("function") => Some(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": clean_schema(tool.get("parameters")),
            }
        })),
        Some("custom") => {
            custom_tools.insert(name.to_string());
            let mut hint = String::from("The complete raw tool input as a single string.");
            if let Some(definition) = tool.pointer("/format/definition").and_then(Value::as_str) {
                hint.push_str(" It must follow this grammar:\n");
                hint.push_str(definition);
            }
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {"input": {"type": "string", "description": hint}},
                        "required": ["input"],
                    },
                }
            }))
        }
        // Hosted tools (web_search, file_search, ...) need OpenAI's servers.
        _ => None,
    }
}

fn input_item(
    item: &Value,
    provider: &ProviderConfig,
    namespaced: &HashMap<String, (String, String)>,
    assistant: &mut PendingAssistant,
    out: &mut Vec<Value>,
) {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match kind {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            if role == "assistant" {
                assistant.text.push_str(&plain_text(item.get("content")));
                return;
            }
            assistant.flush(provider, out);
            let role = if role == "user" { "user" } else { "system" };
            out.push(json!({"role": role, "content": message_content(item.get("content"))}));
        }
        "function_call" => assistant.tool_calls.push(json!({
            "id": item.get("call_id").cloned().unwrap_or(json!("")),
            "type": "function",
            "function": {
                "name": flat_name(item, namespaced),
                "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
            }
        })),
        "custom_tool_call" => assistant.tool_calls.push(json!({
            "id": item.get("call_id").cloned().unwrap_or(json!("")),
            "type": "function",
            "function": {
                "name": item.get("name").cloned().unwrap_or(json!("")),
                "arguments": json!({"input": item.get("input").and_then(Value::as_str).unwrap_or("")}).to_string(),
            }
        })),
        "function_call_output" | "custom_tool_call_output" => {
            assistant.flush(provider, out);
            out.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(json!("")),
                "content": output_text(item.get("output")),
            }));
        }
        "reasoning" => {
            if let Some(text) = reasoning_text(item) {
                assistant.reasoning.push_str(&text);
            }
        }
        "web_search_call" => {
            if let Some(query) = item.pointer("/action/query").and_then(Value::as_str) {
                assistant
                    .text
                    .push_str(&format!("[Searched the web for: {query}]\n"));
            }
        }
        _ => {}
    }
}

/// The provider-side name of a `function_call` item, which names a
/// namespaced function by `namespace` plus `name`.
fn flat_name(item: &Value, namespaced: &HashMap<String, (String, String)>) -> String {
    let name = item["name"].as_str().unwrap_or("");
    match item["namespace"].as_str() {
        Some(namespace) => namespaced
            .iter()
            .find(|(_, (ns, n))| ns == namespace && n == name)
            .map(|(flat, _)| flat.clone())
            .unwrap_or_else(|| name.to_string()),
        None => name.to_string(),
    }
}

fn plain_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn message_content(content: Option<&Value>) -> Value {
    let Some(Value::Array(parts)) = content else {
        return json!(plain_text(content));
    };
    let mut converted = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => {
                converted.push(
                    json!({"type": "text", "text": part.get("text").cloned().unwrap_or(json!(""))}),
                );
            }
            Some("input_image") => {
                let url = part.get("image_url").and_then(|u| {
                    u.as_str()
                        .map(str::to_string)
                        .or_else(|| u.get("url")?.as_str().map(str::to_string))
                });
                if let Some(url) = url {
                    converted.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
            }
            _ => {}
        }
    }
    if converted.iter().all(|p| p["type"] == "text") {
        return json!(converted
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    Value::Array(converted)
}

fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(_)) => plain_text(output),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn reasoning_text(item: &Value) -> Option<String> {
    if let Some(encoded) = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix(REASONING_PREFIX))
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        return String::from_utf8(bytes).ok();
    }
    let text = item
        .get("summary")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|s| s.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[derive(PartialEq)]
enum Open {
    Reasoning { id: String, text: String },
    Message { id: String, text: String },
}

/// Turns neutral events into Responses API SSE events.
pub struct ResponsesEncoder {
    id: String,
    model: String,
    created_at: u64,
    custom_tools: HashSet<String>,
    namespaced: HashMap<String, (String, String)>,
    sequence: u64,
    output: Vec<Value>,
    open: Option<Open>,
    usage: Usage,
    started: bool,
    finished: bool,
}

pub type SseOut = (&'static str, Value);

impl ResponsesEncoder {
    pub fn new(model: &str, custom_tools: HashSet<String>) -> Self {
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id: new_id("resp_"),
            model: model.to_string(),
            created_at,
            custom_tools,
            namespaced: HashMap::new(),
            sequence: 0,
            output: Vec::new(),
            open: None,
            usage: Usage::default(),
            started: false,
            finished: false,
        }
    }

    /// Report calls to namespaced functions by their namespace and name.
    pub fn with_namespaced(mut self, namespaced: HashMap<String, (String, String)>) -> Self {
        self.namespaced = namespaced;
        self
    }

    fn response(&self, status: &str) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": self.output,
            "usage": null,
            "error": null,
            "incomplete_details": null,
        })
    }

    fn emit(&mut self, out: &mut Vec<SseOut>, name: &'static str, mut data: Value) {
        data["type"] = json!(name);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        out.push((name, data));
    }

    pub fn start(&mut self) -> Vec<SseOut> {
        let mut out = Vec::new();
        if self.started {
            return out;
        }
        self.started = true;
        let response = self.response("in_progress");
        self.emit(&mut out, "response.created", json!({"response": response}));
        let response = self.response("in_progress");
        self.emit(
            &mut out,
            "response.in_progress",
            json!({"response": response}),
        );
        out
    }

    fn output_index(&self) -> usize {
        self.output.len()
    }

    fn close(&mut self, out: &mut Vec<SseOut>) {
        let index = self.output_index();
        match self.open.take() {
            Some(Open::Reasoning { id, text }) => {
                let part = json!({"type": "summary_text", "text": text});
                self.emit(
                    out,
                    "response.reasoning_summary_text.done",
                    json!({"item_id": id, "output_index": index, "summary_index": 0, "text": text}),
                );
                self.emit(
                    out,
                    "response.reasoning_summary_part.done",
                    json!({"item_id": id, "output_index": index, "summary_index": 0, "part": part}),
                );
                let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                let item = json!({
                    "type": "reasoning",
                    "id": id,
                    "summary": [part],
                    "encrypted_content": format!("{REASONING_PREFIX}{encoded}"),
                });
                self.emit(
                    out,
                    "response.output_item.done",
                    json!({"output_index": index, "item": item}),
                );
                self.output.push(item);
            }
            Some(Open::Message { id, text }) => {
                let part = json!({"type": "output_text", "text": text, "annotations": []});
                self.emit(
                    out,
                    "response.output_text.done",
                    json!({"item_id": id, "output_index": index, "content_index": 0, "text": text}),
                );
                self.emit(
                    out,
                    "response.content_part.done",
                    json!({"item_id": id, "output_index": index, "content_index": 0, "part": part}),
                );
                let item = json!({
                    "type": "message",
                    "id": id,
                    "role": "assistant",
                    "status": "completed",
                    "content": [part],
                });
                self.emit(
                    out,
                    "response.output_item.done",
                    json!({"output_index": index, "item": item}),
                );
                self.output.push(item);
            }
            None => {}
        }
    }

    pub fn push(&mut self, event: Event) -> Vec<SseOut> {
        let mut out = self.start();
        if self.finished {
            return out;
        }
        let index = self.output_index();
        match event {
            Event::Reasoning(delta) => {
                if !matches!(self.open, Some(Open::Reasoning { .. })) {
                    self.close(&mut out);
                    let id = new_id("rs_");
                    let index = self.output_index();
                    self.emit(&mut out, "response.output_item.added",
                        json!({"output_index": index, "item": {"type": "reasoning", "id": id, "summary": []}}));
                    self.emit(
                        &mut out,
                        "response.reasoning_summary_part.added",
                        json!({"item_id": id, "output_index": index, "summary_index": 0,
                               "part": {"type": "summary_text", "text": ""}}),
                    );
                    self.open = Some(Open::Reasoning {
                        id,
                        text: String::new(),
                    });
                }
                let index = self.output_index();
                if let Some(Open::Reasoning { id, text }) = &mut self.open {
                    text.push_str(&delta);
                    let id = id.clone();
                    self.emit(&mut out, "response.reasoning_summary_text.delta",
                        json!({"item_id": id, "output_index": index, "summary_index": 0, "delta": delta}));
                }
            }
            Event::Text(delta) => {
                if !matches!(self.open, Some(Open::Message { .. })) {
                    self.close(&mut out);
                    let id = new_id("msg_");
                    let index = self.output_index();
                    self.emit(&mut out, "response.output_item.added",
                        json!({"output_index": index, "item": {"type": "message", "id": id, "role": "assistant",
                                                              "status": "in_progress", "content": []}}));
                    self.emit(
                        &mut out,
                        "response.content_part.added",
                        json!({"item_id": id, "output_index": index, "content_index": 0,
                               "part": {"type": "output_text", "text": "", "annotations": []}}),
                    );
                    self.open = Some(Open::Message {
                        id,
                        text: String::new(),
                    });
                }
                let index = self.output_index();
                if let Some(Open::Message { id, text }) = &mut self.open {
                    text.push_str(&delta);
                    let id = id.clone();
                    self.emit(&mut out, "response.output_text.delta",
                        json!({"item_id": id, "output_index": index, "content_index": 0, "delta": delta}));
                }
            }
            Event::ToolCall {
                id: call_id,
                name,
                arguments,
            } => {
                self.close(&mut out);
                let index = self.output_index();
                if self.custom_tools.contains(&name) {
                    let input = serde_json::from_str::<Value>(&arguments)
                        .ok()
                        .and_then(|v| v.get("input").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or(arguments);
                    let item = json!({"type": "custom_tool_call", "id": new_id("ctc_"), "call_id": call_id,
                                      "name": name, "input": input, "status": "completed"});
                    self.emit(
                        &mut out,
                        "response.output_item.added",
                        json!({"output_index": index, "item": item}),
                    );
                    self.emit(
                        &mut out,
                        "response.output_item.done",
                        json!({"output_index": index, "item": item}),
                    );
                    self.output.push(item);
                } else {
                    let item_id = new_id("fc_");
                    let mut item = json!({"type": "function_call", "id": item_id, "call_id": call_id,
                                          "name": name, "arguments": "", "status": "in_progress"});
                    if let Some((namespace, name)) = self.namespaced.get(&name) {
                        item["namespace"] = json!(namespace);
                        item["name"] = json!(name);
                    }
                    self.emit(
                        &mut out,
                        "response.output_item.added",
                        json!({"output_index": index, "item": item}),
                    );
                    self.emit(
                        &mut out,
                        "response.function_call_arguments.delta",
                        json!({"item_id": item_id, "output_index": index, "delta": arguments}),
                    );
                    self.emit(
                        &mut out,
                        "response.function_call_arguments.done",
                        json!({"item_id": item_id, "output_index": index, "arguments": arguments}),
                    );
                    item["arguments"] = json!(arguments);
                    item["status"] = json!("completed");
                    self.emit(
                        &mut out,
                        "response.output_item.done",
                        json!({"output_index": index, "item": item}),
                    );
                    self.output.push(item);
                }
            }
            Event::WebSearch(search) => {
                self.close(&mut out);
                let index = self.output_index();
                let item_id = new_id("ws_");
                let item = json!({"type": "web_search_call", "id": item_id, "status": "in_progress",
                                  "action": {"type": "search", "query": search.query}});
                self.emit(
                    &mut out,
                    "response.output_item.added",
                    json!({"output_index": index, "item": item}),
                );
                for name in [
                    "response.web_search_call.in_progress",
                    "response.web_search_call.searching",
                    "response.web_search_call.completed",
                ] {
                    self.emit(
                        &mut out,
                        name,
                        json!({"item_id": item_id, "output_index": index}),
                    );
                }
                let sources: Vec<Value> = search
                    .outcome
                    .as_ref()
                    .map(|hits| {
                        hits.iter()
                            .map(|hit| json!({"type": "url", "url": hit.url}))
                            .collect()
                    })
                    .unwrap_or_default();
                let item = json!({"type": "web_search_call", "id": item_id, "status": "completed",
                                  "action": {"type": "search", "query": search.query, "sources": sources}});
                self.emit(
                    &mut out,
                    "response.output_item.done",
                    json!({"output_index": index, "item": item}),
                );
                self.output.push(item);
            }
            Event::Usage(usage) => self.usage = usage,
            Event::Finish(finish) => {
                self.close(&mut out);
                let mut response = self.response("completed");
                if finish == Finish::Length {
                    response["incomplete_details"] = json!({"reason": "max_output_tokens"});
                }
                let u = self.usage;
                response["usage"] = json!({
                    "input_tokens": u.input_tokens,
                    "input_tokens_details": {"cached_tokens": u.cached_tokens},
                    "output_tokens": u.output_tokens,
                    "output_tokens_details": {"reasoning_tokens": u.reasoning_tokens},
                    "total_tokens": u.input_tokens + u.output_tokens,
                });
                self.emit(
                    &mut out,
                    "response.completed",
                    json!({"response": response}),
                );
                self.finished = true;
            }
            Event::Error(message) => {
                let _ = index;
                out.extend(self.fail("server_error", &message));
            }
        }
        out
    }

    pub fn fail(&mut self, code: &str, message: &str) -> Vec<SseOut> {
        let mut out = self.start();
        if self.finished {
            return out;
        }
        let mut response = self.response("failed");
        response["error"] = json!({"code": code, "message": message});
        self.emit(&mut out, "response.failed", json!({"response": response}));
        self.finished = true;
        out
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;

    #[test]
    fn converts_codex_history() {
        let mut provider = PRESETS[0].to_provider();
        provider.reasoning_history = true;
        let body = json!({
            "model": "nim/x",
            "instructions": "You are Codex.",
            "tools": [
                {"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}},
                {"type": "custom", "name": "apply_patch", "description": "patch", "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
                {"type": "web_search"}
            ],
            "tool_choice": "auto",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ferry1:dGhpbmtpbmc="},
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
                {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "custom_tool_call_output", "call_id": "c2", "output": "ok"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}
            ]
        });
        let converted = to_chat_request(&body, "x", &provider);
        let chat = converted.request;
        assert!(converted.custom_tools.contains("apply_patch"));
        assert_eq!(chat["tools"].as_array().unwrap().len(), 2);
        let m = chat["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1], json!({"role": "system", "content": "rules"}));
        assert_eq!(m[2], json!({"role": "user", "content": "list files"}));
        assert_eq!(m[3]["tool_calls"][0]["id"], "c1");
        assert_eq!(m[3]["reasoning_content"], "thinking");
        assert_eq!(
            m[4],
            json!({"role": "tool", "tool_call_id": "c1", "content": "a.txt"})
        );
        assert_eq!(
            m[5]["tool_calls"][0]["function"]["arguments"],
            "{\"input\":\"*** Begin Patch\"}"
        );
        assert_eq!(m[6]["tool_call_id"], "c2");
        assert_eq!(m[7]["content"], "Done.");
    }

    #[test]
    fn namespace_tools_round_trip() {
        let provider = crate::presets::PRESETS[0].to_provider();
        let fun = |name: &str| {
            json!({"type": "function", "name": name, "description": name,
                                      "parameters": {"type": "object"}})
        };
        let body = json!({
            "tools": [
                fun("exec_command"),
                {"type": "namespace", "name": "multi_agent_v1", "description": "Sub-agents",
                 "tools": [fun("spawn_agent"), fun("wait_agent"), fun("exec_command")]}
            ],
            "input": [
                {"type": "message", "role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c1", "namespace": "multi_agent_v1",
                 "name": "spawn_agent", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "agent-1"}
            ]
        });
        let converted = to_chat_request(&body, "x", &provider);
        let names: Vec<&str> = converted.request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "exec_command",
                "spawn_agent",
                "wait_agent",
                "multi_agent_v1__exec_command"
            ]
        );
        let call = &converted.request["messages"][1]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "spawn_agent");

        let mut encoder =
            ResponsesEncoder::new("x", HashSet::new()).with_namespaced(converted.namespaced);
        let frames: Vec<SseOut> = [
            Event::ToolCall {
                id: "c2".into(),
                name: "multi_agent_v1__exec_command".into(),
                arguments: "{}".into(),
            },
            Event::ToolCall {
                id: "c3".into(),
                name: "exec_command".into(),
                arguments: "{}".into(),
            },
            Event::Finish(Finish::ToolCalls),
        ]
        .into_iter()
        .flat_map(|e| encoder.push(e))
        .collect();
        let items: Vec<&Value> = frames
            .iter()
            .filter(|(n, _)| *n == "response.output_item.done")
            .map(|(_, d)| &d["item"])
            .collect();
        assert_eq!(items[0]["name"], "exec_command");
        assert_eq!(items[0]["namespace"], "multi_agent_v1");
        assert_eq!(items[1]["name"], "exec_command");
        assert!(items[1].get("namespace").is_none());
    }

    #[test]
    fn encodes_items_in_order() {
        let mut encoder =
            ResponsesEncoder::new("nim/x", HashSet::from(["apply_patch".to_string()]));
        let mut frames = Vec::new();
        for event in [
            Event::Reasoning("hmm".into()),
            Event::Text("Patching".into()),
            Event::ToolCall {
                id: "c9".into(),
                name: "apply_patch".into(),
                arguments: "{\"input\":\"*** Begin Patch\"}".into(),
            },
            Event::ToolCall {
                id: "c10".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            Event::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Usage::default()
            }),
            Event::Finish(Finish::ToolCalls),
        ] {
            frames.extend(encoder.push(event));
        }
        let sequence: Vec<u64> = frames
            .iter()
            .map(|(_, d)| d["sequence_number"].as_u64().unwrap())
            .collect();
        assert!(sequence.windows(2).all(|w| w[1] == w[0] + 1));
        let (name, done) = frames.last().unwrap();
        assert_eq!(*name, "response.completed");
        let output = done["response"]["output"].as_array().unwrap();
        let types: Vec<_> = output.iter().map(|o| o["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            ["reasoning", "message", "custom_tool_call", "function_call"]
        );
        assert_eq!(output[2]["input"], "*** Begin Patch");
        assert_eq!(output[0]["encrypted_content"], "ferry1:aG1t");
        assert_eq!(done["response"]["usage"]["total_tokens"], 7);
    }
}
