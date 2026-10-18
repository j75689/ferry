//! Make Claude Code's auto-mode safety reviews work on non-Claude models.
//!
//! In auto mode, Claude Code asks the model to review risky actions before
//! running them. Stage one allows 64 output tokens and stops at `</block>`;
//! stage two asks for reasoning first. When the reply holds no
//! `<block>yes|no</block>` verdict, Claude Code blocks the action, so tools
//! such as Write and Bash fail with "could not evaluate this action".
//!
//! Reasoning models spend 64 tokens thinking and never reach the verdict, and
//! some providers reject `stop`. A review request gets room to think and loses
//! the stop sequence; Claude Code reads a complete `<block>no</block>` fine.

use serde_json::{json, Value};

/// Output tokens a review request gets at least.
pub const MIN_REVIEW_TOKENS: u64 = 4096;

/// Stop sequences that only end a verdict tag.
const VERDICT_STOPS: [&str; 2] = ["</block>", "</severity>"];

fn texts(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whether the request is one of Claude Code's auto-mode safety reviews.
pub fn is_review(body: &Value) -> bool {
    if body["tools"].as_array().is_some_and(|t| !t.is_empty()) {
        return false;
    }
    let transcript = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|m| m["role"] == "user" && texts(&m["content"]).contains("<transcript>"));
    if !transcript {
        return false;
    }
    let system = texts(&body["system"]);
    system.contains("<block>no</block>") || system.contains("Output <severity>N</severity>")
}

/// Adjust a review request in place. Returns whether it was one.
pub fn prepare(body: &mut Value) -> bool {
    if !is_review(body) {
        return false;
    }
    if let Some(max) = body["max_tokens"].as_u64() {
        body["max_tokens"] = json!(max.max(MIN_REVIEW_TOKENS));
    }
    if let Some(stops) = body.get_mut("stop_sequences").and_then(Value::as_array_mut) {
        stops.retain(|s| !s.as_str().is_some_and(|s| VERDICT_STOPS.contains(&s)));
        if stops.is_empty() {
            body.as_object_mut().unwrap().remove("stop_sequences");
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_one() -> Value {
        json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "stop_sequences": ["</block>"],
            "thinking": {"type": "disabled"},
            "system": [
                {"type": "text", "text": "You are a security monitor."},
                {"type": "text", "text": "If the action should be allowed:\n<block>no</block>"}
            ],
            "messages": [{"role": "user", "content": [{"type": "text",
                "text": "<transcript>\n{\"Bash\":\"touch /tmp/x\"}\n</transcript>\nYour ENTIRE response MUST begin with <block>."}]}]
        })
    }

    #[test]
    fn gives_reviews_room_and_drops_the_verdict_stop() {
        let mut body = stage_one();
        assert!(prepare(&mut body));
        assert_eq!(body["max_tokens"], MIN_REVIEW_TOKENS);
        assert!(body.get("stop_sequences").is_none());

        let mut body = stage_one();
        body["max_tokens"] = json!(8192);
        body["stop_sequences"] = json!(["</block>", "STOP"]);
        assert!(prepare(&mut body));
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["stop_sequences"], json!(["STOP"]));
    }

    #[test]
    fn leaves_other_requests_alone() {
        let mut with_tools = stage_one();
        with_tools["tools"] = json!([{"name": "Bash"}]);
        assert!(!prepare(&mut with_tools));
        assert_eq!(with_tools["max_tokens"], 64);

        let mut no_transcript = stage_one();
        no_transcript["messages"] =
            json!([{"role": "user", "content": "explain <block>no</block>"}]);
        assert!(!prepare(&mut no_transcript));

        let mut other_system = stage_one();
        other_system["system"] = json!("You are a helpful assistant.");
        assert!(!prepare(&mut other_system));
    }
}
