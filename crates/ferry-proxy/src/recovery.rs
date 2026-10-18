//! Resuming a response whose stream broke after the client already got part of it.
//!
//! The partial answer is replayed as an assistant turn and the model is asked
//! to continue; models often restart the last sentence, so the resumed text
//! is de-duplicated against what was already delivered.

use serde_json::{json, Value};

/// Most continuation attempts per client request.
pub const MAX_RECOVERIES: u32 = 2;

const CONTINUE_PROMPT: &str = "Your previous reply was cut off by a network error. \
Continue exactly where it stopped. Do not repeat anything already written and do not \
comment on the interruption.";

/// Characters of resumed text inspected before deciding how much overlaps.
const PROBE_CHARS: usize = 200;
/// Characters of delivered text the resumed text is compared against.
const TAIL_CHARS: usize = 1000;
/// Shorter matches are too likely to be coincidence (a space, "the ").
const MIN_OVERLAP_CHARS: usize = 8;
/// A repeat that starts at a word or line boundary is a restart of the
/// cut-off word, so far less is needed to trust it.
const MIN_BOUNDARY_OVERLAP_CHARS: usize = 3;

/// Extend an OpenAI Chat request so the model continues `delivered`.
pub fn continuation_request(mut request: Value, delivered: &str) -> Value {
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(json!({"role": "assistant", "content": delivered}));
        messages.push(json!({"role": "user", "content": CONTINUE_PROMPT}));
    }
    request
}

fn tail(text: &str, chars: usize) -> &str {
    match text.char_indices().rev().nth(chars.saturating_sub(1)) {
        Some((index, _)) if chars > 0 => &text[index..],
        _ => text,
    }
}

/// Bytes at the start of `incoming` that repeat the end of `delivered`.
pub fn overlap(delivered: &str, incoming: &str) -> usize {
    let tail = tail(delivered, TAIL_CHARS);
    let tail_is_whole = tail.len() == delivered.len();
    // Longest suffix of `tail` that `incoming` starts with.
    for (start, _) in tail.char_indices() {
        let suffix = &tail[start..];
        let at_boundary = match tail[..start].chars().next_back() {
            Some(previous) => previous.is_whitespace(),
            None => tail_is_whole,
        };
        let needed = if at_boundary {
            MIN_BOUNDARY_OVERLAP_CHARS
        } else {
            MIN_OVERLAP_CHARS
        };
        let length = suffix.chars().count();
        if length < MIN_BOUNDARY_OVERLAP_CHARS {
            break;
        }
        if length < needed {
            continue;
        }
        if incoming.starts_with(suffix) {
            return suffix.len();
        }
        // The resumed text only repeated part of the tail, then ended.
        if suffix.starts_with(incoming) && incoming.chars().count() >= needed {
            return incoming.len();
        }
    }
    0
}

/// Holds back the first part of a resumed stream until the overlap with the
/// delivered text can be judged, then passes everything through.
pub struct OverlapTrimmer {
    delivered_tail: String,
    buffer: String,
    decided: bool,
}

impl OverlapTrimmer {
    pub fn new(delivered: &str) -> Self {
        Self {
            delivered_tail: tail(delivered, TAIL_CHARS).to_string(),
            buffer: String::new(),
            decided: false,
        }
    }

    /// Text that is safe to deliver now (possibly empty).
    pub fn push(&mut self, text: &str) -> String {
        if self.decided {
            return text.to_string();
        }
        self.buffer.push_str(text);
        if self.buffer.chars().count() < PROBE_CHARS {
            return String::new();
        }
        self.decide()
    }

    /// Release whatever is still held back.
    pub fn finish(&mut self) -> String {
        if self.decided {
            return String::new();
        }
        self.decide()
    }

    fn decide(&mut self) -> String {
        self.decided = true;
        let buffer = std::mem::take(&mut self.buffer);
        let mut skip = overlap(&self.delivered_tail, &buffer);
        if skip == 0 {
            // A resumed reply sometimes restarts after stray whitespace.
            let trimmed = buffer.trim_start();
            let lead = buffer.len() - trimmed.len();
            let inner = overlap(&self.delivered_tail, trimmed);
            if inner > 0 {
                skip = lead + inner;
            }
        }
        buffer[skip..].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_appends_partial_answer_and_prompt() {
        let request = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let resumed = continuation_request(request, "Half an ans");
        let messages = resumed["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": "Half an ans"})
        );
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn finds_restarted_sentence() {
        let delivered = "First sentence. The quick brown fo";
        assert_eq!(
            overlap(delivered, "The quick brown fox jumps."),
            "The quick brown fo".len()
        );
        assert_eq!(overlap(delivered, "x jumps."), 0);
        // Too short to trust.
        assert_eq!(overlap("ends with a", "a new start"), 0);
        assert_eq!(overlap("cut mid-wo", "wo-rd"), 0);
        // A restarted word or line needs only a few characters.
        assert_eq!(overlap("Line one.\nLine tw", "Line two."), "Line tw".len());
        assert_eq!(overlap("I agree. The", "The answer is 4."), "The".len());
    }

    #[test]
    fn handles_multibyte_text() {
        let delivered = "Ünïcödé text — résumé naïve café, stopped hëre";
        let incoming = "résumé naïve café, stopped hëre and continües";
        let skip = overlap(delivered, incoming);
        assert_eq!(&incoming[skip..], " and continües");
    }

    #[test]
    fn trimmer_buffers_then_passes_through() {
        let delivered = "Step one is done. Now the second step is to run the tes";
        let mut trimmer = OverlapTrimmer::new(delivered);
        assert_eq!(trimmer.push("Now the second step is"), "");
        assert_eq!(trimmer.finish(), "");
        let mut trimmer = OverlapTrimmer::new(delivered);
        let mut out = trimmer.push("  the second step is to run the tests");
        out.push_str(&trimmer.push(&" and check output".repeat(12)));
        assert!(out.starts_with("ts and check output"), "{out}");
        assert_eq!(trimmer.push(" tail"), " tail");
    }
}
