//! Answer Claude Code's housekeeping requests without calling a provider.
//!
//! Each turn, interactive Claude Code asks the main model for a session
//! title, which doubles the request count against rate-limited free tiers.
//! Only requests matched by narrow, specific markers are answered locally.

use serde_json::{json, Value};

use crate::config::Optimizations;

/// A locally produced reply: what kind of request it was, and the text.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalReply {
    pub kind: &'static str,
    pub text: String,
}

const TITLE_MARKER: &str = "You are naming a coding session";
const MAX_TITLE_WORDS: usize = 6;
const MAX_TITLE_CHARS: usize = 60;

pub fn answer(body: &Value, optimizations: &Optimizations) -> Option<LocalReply> {
    if optimizations.local_session_titles {
        if let Some(session) = title_request(body) {
            return Some(LocalReply {
                kind: "session title",
                text: json!({"title": session_title(&session)}).to_string(),
            });
        }
    }
    if optimizations.local_quota_probe && is_quota_probe(body) {
        return Some(LocalReply {
            kind: "quota probe",
            text: "quota".into(),
        });
    }
    None
}

fn texts(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => Vec::new(),
    }
}

/// The `<session>` content of a session-title request.
fn title_request(body: &Value) -> Option<String> {
    let has_tools = body["tools"].as_array().is_some_and(|t| !t.is_empty());
    let wants_title = body
        .pointer("/output_config/format/schema/properties/title")
        .is_some();
    let named = texts(&body["system"])
        .iter()
        .any(|t| t.contains(TITLE_MARKER));
    if has_tools || !wants_title || !named {
        return None;
    }
    let last = body["messages"].as_array()?.last()?;
    let text = texts(&last["content"]).join("\n");
    let start = text.find("<session>")? + "<session>".len();
    let end = text[start..]
        .find("</session>")
        .map_or(text.len(), |e| start + e);
    Some(text[start..end].trim().to_string())
}

/// A short title made from the start of what the user wrote.
pub fn session_title(session: &str) -> String {
    let first_line = session
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let words: Vec<&str> = first_line
        .split_whitespace()
        .filter(|w| !w.starts_with("http://") && !w.starts_with("https://"))
        .take(MAX_TITLE_WORDS)
        .collect();
    let mut title: String = words.join(" ");
    if title.chars().count() > MAX_TITLE_CHARS {
        title = title.chars().take(MAX_TITLE_CHARS).collect();
    }
    let title = title.trim_end_matches(|c: char| c.is_ascii_punctuation() && c != ')' && c != '`');
    let mut chars = title.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "New session".into(),
    }
}

/// Older Claude Code versions check the quota with a one-token "quota" call.
fn is_quota_probe(body: &Value) -> bool {
    let Some(messages) = body["messages"].as_array() else {
        return false;
    };
    body["max_tokens"].as_u64() == Some(1)
        && messages.len() == 1
        && messages[0]["role"] == "user"
        && texts(&messages[0]["content"]).join("").trim() == "quota"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title_body(session: &str) -> Value {
        json!({
            "model": "claude-haiku-4-5",
            "max_tokens": 128000,
            "stream": true,
            "tools": [],
            "output_config": {"format": {"type": "json_schema", "schema": {
                "type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]}}},
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "You are naming a coding session so the user can pick it out of a long list."}
            ],
            "messages": [{"role": "user", "content": [{"type": "text",
                "text": format!("<session>\n{session}\n</session>\n\nWrite the title in the predominant language of the session.")}]}]
        })
    }

    #[test]
    fn answers_session_title_requests() {
        let on = Optimizations::default();
        let reply = answer(&title_body("hello there, summarize main.py"), &on).unwrap();
        assert_eq!(reply.kind, "session title");
        assert_eq!(reply.text, r#"{"title":"Hello there, summarize main.py"}"#);

        let off = Optimizations {
            local_session_titles: false,
            ..Optimizations::default()
        };
        assert!(answer(&title_body("x"), &off).is_none());
    }

    #[test]
    fn leaves_normal_turns_alone() {
        let on = Optimizations::default();
        let mut body = title_body("hi");
        body["tools"] = json!([{"name": "Bash"}]);
        assert!(answer(&body, &on).is_none());
        let plain = json!({"max_tokens": 100, "messages": [{"role": "user", "content": "You are naming a coding session"}]});
        assert!(answer(&plain, &on).is_none());
        let not_quota =
            json!({"max_tokens": 1, "messages": [{"role": "user", "content": "quota please"}]});
        assert!(answer(&not_quota, &on).is_none());
    }

    #[test]
    fn answers_quota_probes() {
        let probe = json!({"max_tokens": 1, "messages": [{"role": "user", "content": "quota"}]});
        assert_eq!(
            answer(&probe, &Optimizations::default()).unwrap().kind,
            "quota probe"
        );
    }

    #[test]
    fn titles_are_short_and_clean() {
        assert_eq!(session_title(""), "New session");
        assert_eq!(
            session_title(
                "see https://github.com/a/b/pull/1 please review the parser changes now thanks"
            ),
            "See please review the parser changes"
        );
        assert_eq!(
            session_title("διόρθωση σύνδεσης χρήστη"),
            "Διόρθωση σύνδεσης χρήστη"
        );
        assert_eq!(session_title("fix `parse_args()`."), "Fix `parse_args()`");
    }
}
