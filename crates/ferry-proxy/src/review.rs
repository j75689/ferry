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
//!
//! Some models state the verdict another way (`shouldBlock: false`,
//! `Block: no`) or only in their reasoning. [`VerdictFixer`] then appends the
//! tag the reply meant. A reply with no recognisable verdict is left alone,
//! so Claude Code still blocks the action.

use serde_json::{json, Value};

use crate::stream::Event;

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

/// Watches a `<block>` review reply and, when it finishes, adds the verdict
/// tag if the model gave its verdict in another form.
#[derive(Debug, Default)]
pub struct VerdictFixer {
    text: String,
    reasoning: String,
    /// How the reply ended, for the request log.
    pub outcome: Option<&'static str>,
}

impl VerdictFixer {
    /// A fixer for a request that expects a `<block>` verdict.
    pub fn for_request(body: &Value) -> Option<Self> {
        (is_review(body) && texts(&body["system"]).contains("<block>no</block>"))
            .then(Self::default)
    }

    pub fn push(&mut self, event: Event) -> Vec<Event> {
        match &event {
            Event::Text(text) => self.text.push_str(text),
            Event::Reasoning(text) => self.reasoning.push_str(text),
            Event::Finish(_) => {
                if let Some(blocked) = tagged_verdict(&self.text) {
                    self.outcome = Some(if blocked { "block" } else { "allow" });
                } else if let Some(blocked) =
                    stated_verdict(&self.text).or_else(|| stated_verdict(&self.reasoning))
                {
                    self.outcome = Some(if blocked {
                        "block, tag added"
                    } else {
                        "allow, tag added"
                    });
                    let tag = if blocked { "yes" } else { "no" };
                    return vec![Event::Text(format!("\n<block>{tag}</block>")), event];
                } else {
                    self.outcome = Some("no verdict");
                }
            }
            _ => {}
        }
        vec![event]
    }
}

/// The verdict of a `<block>yes|no` tag, the way Claude Code reads it.
fn tagged_verdict(text: &str) -> Option<bool> {
    last_value(
        &text.to_ascii_lowercase(),
        "<block>",
        false,
        &[("yes", true), ("no", false)],
    )
    .map(|(_, v)| v)
}

/// A verdict stated as a tag, `shouldBlock: true`, `block: no`, or
/// `verdict: allow`, whichever comes last.
fn stated_verdict(text: &str) -> Option<bool> {
    let lower = text.to_ascii_lowercase();
    let candidates = [
        last_value(&lower, "<block>", false, &[("yes", true), ("no", false)]),
        last_value(
            &lower,
            "shouldblock",
            true,
            &[("true", true), ("false", false)],
        ),
        last_value(&lower, "block", true, &[("yes", true), ("no", false)]),
        last_value(
            &lower,
            "verdict",
            true,
            &[("block", true), ("allow", false)],
        ),
    ];
    candidates
        .into_iter()
        .flatten()
        .max_by_key(|(at, _)| *at)
        .map(|(_, v)| v)
}

/// The last `key` followed by one of `words`, with its position. With
/// `separated`, a `:` or `=` must sit between them (quotes, `*` and spaces
/// are skipped), and `key` must start a word.
fn last_value(
    lower: &str,
    key: &str,
    separated: bool,
    words: &[(&str, bool)],
) -> Option<(usize, bool)> {
    let mut found = None;
    for (at, _) in lower.match_indices(key) {
        if separated
            && lower[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let rest = &lower[at + key.len()..];
        let skipped = rest.trim_start_matches(|c: char| c.is_whitespace() || "\"'*:=".contains(c));
        let gap = &rest[..rest.len() - skipped.len()];
        if separated && !gap.contains([':', '=']) {
            continue;
        }
        for (word, value) in words {
            let ends = skipped[word.len().min(skipped.len())..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
            if skipped.starts_with(word) && ends {
                found = Some((at, *value));
            }
        }
    }
    found
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

    fn verdict(text: &str, reasoning: &str) -> (Vec<Event>, Option<&'static str>) {
        let mut fixer = VerdictFixer::for_request(&stage_one()).unwrap();
        fixer.push(Event::Reasoning(reasoning.into()));
        fixer.push(Event::Text(text.into()));
        let out = fixer.push(Event::Finish(crate::stream::Finish::Stop));
        (out, fixer.outcome)
    }

    #[test]
    fn supplies_the_verdict_tag_the_model_meant() {
        let (out, outcome) = verdict("Looks fine. <block>no</block>", "");
        assert_eq!(out.len(), 1);
        assert_eq!(outcome, Some("allow"));

        let (out, outcome) = verdict("```json\n{\"shouldBlock\": false}\n```", "");
        assert_eq!(out[0], Event::Text("\n<block>no</block>".into()));
        assert_eq!(outcome, Some("allow, tag added"));

        let (out, _) = verdict("**Block:** yes. Deletes the home directory.", "");
        assert_eq!(out[0], Event::Text("\n<block>yes</block>".into()));

        let (out, _) = verdict("", "The user asked for it. <block>no</block>");
        assert_eq!(out[0], Event::Text("\n<block>no</block>".into()));

        // The last statement wins.
        let (out, _) = verdict(
            "Block: yes at first glance, but on reflection Verdict: allow",
            "",
        );
        assert_eq!(out[0], Event::Text("\n<block>no</block>".into()));
    }

    #[test]
    fn leaves_replies_without_a_verdict_alone() {
        for text in [
            "I am not sure.",
            "This does not block anything",
            "unblock: no",
            "Block nothing",
        ] {
            let (out, outcome) = verdict(text, "");
            assert_eq!(out.len(), 1, "{text}");
            assert_eq!(outcome, Some("no verdict"));
        }
        let severity = json!({"messages": [{"role": "user", "content": "<transcript>x</transcript>"}],
            "system": "Output <severity>N</severity>"});
        assert!(VerdictFixer::for_request(&severity).is_none());
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
