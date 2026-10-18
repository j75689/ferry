//! Provider-neutral stream events, SSE framing, and the OpenAI Chat decoder.

use std::collections::BTreeMap;

use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
}

/// What every upstream stream is reduced to before re-encoding for a client.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Reasoning(String),
    Text(String),
    /// Tool calls are delivered whole, after their arguments finished streaming.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage(Usage),
    Finish(Finish),
    Error(String),
}

/// One `event:`/`data:` frame.
#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental Server-Sent Events parser over arbitrary byte chunks.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            self.line(&line, &mut frames);
        }
        frames
    }

    /// Flush a final frame that was not followed by a blank line.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if !self.buffer.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            self.line(rest.trim_end_matches('\r'), &mut frames);
        }
        self.dispatch(&mut frames);
        frames
    }

    fn line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
        if line.is_empty() {
            self.dispatch(frames);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, frames: &mut Vec<SseFrame>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        frames.push(SseFrame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        });
    }
}

/// Splits a leading `<think>...</think>` section out of streamed content.
#[derive(Default)]
struct ThinkSplitter {
    state: ThinkState,
    pending: String,
}

#[derive(Default, PartialEq)]
enum ThinkState {
    #[default]
    Undecided,
    Thinking,
    Text,
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

impl ThinkSplitter {
    fn push(&mut self, chunk: &str, out: &mut Vec<Event>) {
        self.pending.push_str(chunk);
        loop {
            match self.state {
                ThinkState::Text => {
                    if !self.pending.is_empty() {
                        out.push(Event::Text(std::mem::take(&mut self.pending)));
                    }
                    return;
                }
                ThinkState::Undecided => {
                    let trimmed = self.pending.trim_start();
                    if trimmed.starts_with(OPEN) {
                        let start = self.pending.len() - trimmed.len() + OPEN.len();
                        self.pending.drain(..start);
                        self.state = ThinkState::Thinking;
                    } else if OPEN.starts_with(trimmed) {
                        return; // could still become `<think>`
                    } else {
                        self.state = ThinkState::Text;
                    }
                }
                ThinkState::Thinking => {
                    if let Some(pos) = self.pending.find(CLOSE) {
                        let reasoning: String = self.pending.drain(..pos).collect();
                        self.pending.drain(..CLOSE.len());
                        if !reasoning.is_empty() {
                            out.push(Event::Reasoning(reasoning));
                        }
                        let rest = self.pending.trim_start().to_string();
                        self.pending = rest;
                        self.state = ThinkState::Text;
                        continue;
                    }
                    // Hold back a suffix that might be the start of `</think>`.
                    let keep = (1..CLOSE.len())
                        .rev()
                        .find(|&n| self.pending.ends_with(&CLOSE[..n]))
                        .unwrap_or(0);
                    let emit_len = self.pending.len() - keep;
                    if emit_len > 0 {
                        let reasoning: String = self.pending.drain(..emit_len).collect();
                        out.push(Event::Reasoning(reasoning));
                    }
                    return;
                }
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        let rest = std::mem::take(&mut self.pending);
        if rest.is_empty() {
            return;
        }
        match self.state {
            ThinkState::Thinking => out.push(Event::Reasoning(rest)),
            _ => out.push(Event::Text(rest)),
        }
    }
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

/// Turns one provider's SSE frames into [`Event`]s.
pub trait FrameDecoder {
    fn push_frame(&mut self, frame: &SseFrame) -> Vec<Event>;
    /// The body ended; report an error unless the response had finished.
    fn end_of_body(&mut self) -> Vec<Event>;
}

impl FrameDecoder for ChatStreamDecoder {
    fn push_frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        ChatStreamDecoder::push_frame(self, frame)
    }
    fn end_of_body(&mut self) -> Vec<Event> {
        ChatStreamDecoder::end_of_body(self)
    }
}

#[derive(Default)]
struct OpenTool {
    id: String,
    name: String,
    arguments: String,
}

/// Reduces Anthropic Messages SSE (`message_start` ... `message_stop`) to [`Event`]s.
#[derive(Default)]
pub struct MessagesStreamDecoder {
    tool: Option<OpenTool>,
    usage: Usage,
    finish: Option<Finish>,
    done: bool,
}

impl FrameDecoder for MessagesStreamDecoder {
    fn push_frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        let Ok(data) = serde_json::from_str::<Value>(&frame.data) else {
            return out;
        };
        let num = |v: &Value| v.as_u64().unwrap_or(0);
        match data["type"].as_str().unwrap_or("") {
            "message_start" => {
                let usage = &data["message"]["usage"];
                self.usage.input_tokens = num(&usage["input_tokens"]);
                self.usage.cached_tokens = num(&usage["cache_read_input_tokens"]);
            }
            "content_block_start" => {
                let block = &data["content_block"];
                if block["type"] == "tool_use" {
                    self.tool = Some(OpenTool {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        arguments: String::new(),
                    });
                }
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(Event::Text(text.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta["thinking"].as_str().filter(|t| !t.is_empty()) {
                            out.push(Event::Reasoning(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(tool), Some(part)) =
                            (&mut self.tool, delta["partial_json"].as_str())
                        {
                            tool.arguments.push_str(part);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(tool) = self.tool.take() {
                    out.push(Event::ToolCall {
                        id: if tool.id.is_empty() {
                            new_id("toolu_")
                        } else {
                            tool.id
                        },
                        name: tool.name,
                        arguments: if tool.arguments.trim().is_empty() {
                            "{}".into()
                        } else {
                            tool.arguments
                        },
                    });
                }
            }
            "message_delta" => {
                if let Some(output) = data["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = output;
                }
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.finish = Some(match reason {
                        "max_tokens" => Finish::Length,
                        "tool_use" => Finish::ToolCalls,
                        "refusal" => Finish::ContentFilter,
                        _ => Finish::Stop,
                    });
                }
            }
            "message_stop" => {
                self.done = true;
                out.push(Event::Usage(self.usage));
                out.push(Event::Finish(self.finish.unwrap_or(Finish::Stop)));
            }
            "error" => {
                self.done = true;
                out.push(Event::Error(error_message(&data["error"])));
            }
            _ => {}
        }
        out
    }

    fn end_of_body(&mut self) -> Vec<Event> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        vec![
            Event::Usage(self.usage),
            Event::Error("provider stream ended before the response finished".into()),
        ]
    }
}

/// Reduces OpenAI `chat.completion.chunk` objects to [`Event`]s.
#[derive(Default)]
pub struct ChatStreamDecoder {
    think: ThinkSplitter,
    tools: BTreeMap<u64, PendingTool>,
    finish: Option<Finish>,
    usage: Option<Usage>,
    done: bool,
}

impl ChatStreamDecoder {
    pub fn push_frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        let data = frame.data.trim();
        if data == "[DONE]" {
            return self.finish();
        }
        match serde_json::from_str::<Value>(data) {
            Ok(chunk) => self.push(&chunk),
            Err(_) => Vec::new(),
        }
    }

    pub fn push(&mut self, chunk: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        if let Some(error) = chunk.get("error") {
            out.push(Event::Error(error_message(error)));
            return out;
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(parse_chat_usage(usage));
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return out;
        };
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        if let Some(delta) = delta {
            for key in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta.get(key).and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(Event::Reasoning(text.to_string()));
                    }
                }
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                self.think.push(text, &mut out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, call) in calls.iter().enumerate() {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(position as u64);
                    let tool = self.tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            tool.id = id.to_string();
                        }
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            tool.name.push_str(name);
                        }
                        match function.get("arguments") {
                            Some(Value::String(args)) => tool.arguments.push_str(args),
                            Some(other @ Value::Object(_)) => {
                                tool.arguments.push_str(&other.to_string())
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(match reason {
                "length" => Finish::Length,
                "tool_calls" | "function_call" => Finish::ToolCalls,
                "content_filter" => Finish::ContentFilter,
                _ => Finish::Stop,
            });
        }
        out
    }

    /// The response body ended. Without `[DONE]` or a `finish_reason` the
    /// stream was cut off, which callers must not mistake for a full answer.
    pub fn end_of_body(&mut self) -> Vec<Event> {
        if self.done || self.finish.is_some() {
            return self.finish();
        }
        self.done = true;
        let mut out = Vec::new();
        self.think.finish(&mut out);
        // Partial answers are still billed; keep the count.
        if let Some(usage) = self.usage {
            out.push(Event::Usage(usage));
        }
        out.push(Event::Error(
            "provider stream ended before the response finished".into(),
        ));
        out
    }

    /// Flush buffered content; safe to call more than once.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.done = true;
        self.think.finish(&mut out);
        let mut had_tools = false;
        for (_, tool) in std::mem::take(&mut self.tools) {
            if tool.name.is_empty() {
                continue;
            }
            had_tools = true;
            let arguments = if tool.arguments.trim().is_empty() {
                "{}".to_string()
            } else {
                tool.arguments
            };
            out.push(Event::ToolCall {
                id: if tool.id.is_empty() {
                    new_id("call_")
                } else {
                    tool.id
                },
                name: tool.name,
                arguments,
            });
        }
        if let Some(usage) = self.usage {
            out.push(Event::Usage(usage));
        }
        let finish = match self.finish {
            Some(Finish::Stop) | None if had_tools => Finish::ToolCalls,
            Some(finish) => finish,
            None => Finish::Stop,
        };
        out.push(Event::Finish(finish));
        out
    }
}

pub fn parse_chat_usage(usage: &Value) -> Usage {
    let num = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: num(usage.get("prompt_tokens")),
        output_tokens: num(usage.get("completion_tokens")),
        cached_tokens: num(usage.pointer("/prompt_tokens_details/cached_tokens")),
        reasoning_tokens: num(usage.pointer("/completion_tokens_details/reasoning_tokens")),
    }
}

pub fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| error.as_str().map(str::to_string))
        .unwrap_or_else(|| error.to_string())
}

pub fn new_id(prefix: &str) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let suffix: String = (0..24)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{prefix}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(chunks: &[Value]) -> Vec<Event> {
        let mut decoder = ChatStreamDecoder::default();
        let mut events: Vec<Event> = chunks.iter().flat_map(|c| decoder.push(c)).collect();
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn sse_parser_handles_split_frames() {
        let mut parser = SseParser::default();
        let mut frames = parser.push(b"event: a\r\ndata: {\"x\"");
        assert!(frames.is_empty());
        frames.extend(parser.push(b":1}\r\n\r\n: comment\ndata: [DONE]\n\n"));
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[0].data, "{\"x\":1}");
        assert_eq!(frames[1].data, "[DONE]");
    }

    #[test]
    fn tool_call_fragments_are_reassembled() {
        let events = run(&[
            json!({"choices":[{"delta":{"content":"Let me look."}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Read","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ]);
        assert_eq!(
            events,
            vec![
                Event::Text("Let me look.".into()),
                Event::ToolCall {
                    id: "call_1".into(),
                    name: "Read".into(),
                    arguments: "{\"path\":\"a\"}".into()
                },
                Event::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Usage::default()
                }),
                Event::Finish(Finish::ToolCalls),
            ]
        );
    }

    #[test]
    fn think_tags_become_reasoning_across_chunks() {
        let events = run(&[
            json!({"choices":[{"delta":{"content":"  <thi"}}]}),
            json!({"choices":[{"delta":{"content":"nk>plan it</th"}}]}),
            json!({"choices":[{"delta":{"content":"ink>\n\nAnswer"}}]}),
        ]);
        assert_eq!(
            events,
            vec![
                Event::Reasoning("plan it".into()),
                Event::Text("Answer".into()),
                Event::Finish(Finish::Stop),
            ]
        );
    }

    #[test]
    fn reasoning_content_field_and_plain_text() {
        let events = run(&[
            json!({"choices":[{"delta":{"reasoning_content":"hmm"}}]}),
            json!({"choices":[{"delta":{"content":"a <b> c"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
        ]);
        assert_eq!(
            events,
            vec![
                Event::Reasoning("hmm".into()),
                Event::Text("a <b> c".into()),
                Event::Finish(Finish::Length),
            ]
        );
    }

    #[test]
    fn decodes_anthropic_messages_stream() {
        let frames = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":4}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Running."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"[\"ls\"]}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ];
        let mut decoder = MessagesStreamDecoder::default();
        let events: Vec<Event> = frames
            .iter()
            .flat_map(|f| {
                decoder.push_frame(&SseFrame {
                    event: None,
                    data: f.to_string(),
                })
            })
            .collect();
        assert_eq!(
            events,
            vec![
                Event::Reasoning("plan".into()),
                Event::Text("Running.".into()),
                Event::ToolCall {
                    id: "toolu_1".into(),
                    name: "shell".into(),
                    arguments: "{\"command\":[\"ls\"]}".into()
                },
                Event::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 9,
                    cached_tokens: 4,
                    reasoning_tokens: 0
                }),
                Event::Finish(Finish::ToolCalls),
            ]
        );
        assert!(decoder.end_of_body().is_empty());
        let mut cut = MessagesStreamDecoder::default();
        assert!(matches!(cut.end_of_body().last(), Some(Event::Error(_))));
    }

    #[test]
    fn body_ending_without_finish_is_an_error() {
        let mut decoder = ChatStreamDecoder::default();
        decoder.push(&json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}));
        let events = decoder.end_of_body();
        assert!(matches!(events.last(), Some(Event::Error(_))));
        let mut decoder = ChatStreamDecoder::default();
        decoder.push(&json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}));
        assert_eq!(
            decoder.end_of_body().last(),
            Some(&Event::Finish(Finish::Stop))
        );
    }

    #[test]
    fn stop_with_tools_reports_tool_calls() {
        let events = run(&[
            json!({"choices":[{"delta":{"tool_calls":[{"function":{"name":"Bash","arguments":""}}]},"finish_reason":"stop"}]}),
        ]);
        assert!(matches!(&events[0], Event::ToolCall { arguments, .. } if arguments == "{}"));
        assert_eq!(events.last(), Some(&Event::Finish(Finish::ToolCalls)));
    }
}
