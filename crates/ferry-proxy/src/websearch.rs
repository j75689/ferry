//! Hosted web search for providers that have none.
//!
//! Claude Code's WebSearch and Codex's `web_search` are server-side tools:
//! the API provider searches and the model reads the results within one
//! response. Ferry offers the model a `web_search` function instead, runs the
//! search itself when it is called, reports the search to the client in its
//! own format, and asks the model to continue with the results.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::config::{SearchBackend, WebSearchConfig};
use crate::stream::{new_id, Event, Finish, Usage};
use crate::upstream::{EventStream, UpstreamError};

/// Name of the function the model calls to search.
pub const TOOL_NAME: &str = "web_search";
/// Rounds of searching before the model must answer with what it has.
const MAX_ROUNDS: usize = 5;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const DUCKDUCKGO_URL: &str = "https://lite.duckduckgo.com/lite/";
const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; Ferry)";

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// How old the page is, as the backend words it ("2 days ago").
    pub age: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// Anthropic's error code: `unavailable`, `max_uses_exceeded`, ...
    pub code: &'static str,
    pub message: String,
}

/// One search Ferry ran, as reported to the client.
#[derive(Debug, Clone, PartialEq)]
pub struct Search {
    pub id: String,
    pub query: String,
    pub outcome: Result<Vec<Hit>, Failure>,
}

/// What the client's web search tool allows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub max_uses: Option<u32>,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d.as_str().map(|d| d.trim().to_ascii_lowercase()))
        .filter(|d| !d.is_empty())
        .collect()
}

fn has_function(tools: &[Value], name: &str) -> bool {
    tools.iter().any(|t| {
        t.get("name").and_then(Value::as_str) == Some(name)
            && t.get("type")
                .and_then(Value::as_str)
                .is_none_or(|k| k == "function" || k == "custom")
    })
}

/// The web search plan of an Anthropic Messages request
/// (`{"type": "web_search_20250305", "name": "web_search", ...}`).
pub fn messages_plan(body: &Value) -> Option<Plan> {
    let tools = body["tools"].as_array()?;
    let tool = tools.iter().find(|t| {
        t["type"]
            .as_str()
            .is_some_and(|k| k.starts_with("web_search_"))
    })?;
    if has_function(tools, TOOL_NAME) {
        return None;
    }
    Some(Plan {
        max_uses: tool["max_uses"].as_u64().map(|n| n as u32),
        allowed_domains: strings(&tool["allowed_domains"]),
        blocked_domains: strings(&tool["blocked_domains"]),
    })
}

/// The web search plan of a Responses request (`{"type": "web_search"}`).
pub fn responses_plan(body: &Value) -> Option<Plan> {
    let tools = body["tools"].as_array()?;
    let tool = tools.iter().find(|t| {
        matches!(
            t["type"].as_str(),
            Some("web_search" | "web_search_preview")
        )
    })?;
    if has_function(tools, TOOL_NAME) {
        return None;
    }
    Some(Plan {
        max_uses: None,
        allowed_domains: strings(
            &tool
                .pointer("/filters/allowed_domains")
                .cloned()
                .unwrap_or_default(),
        ),
        blocked_domains: Vec::new(),
    })
}

/// Offer the search function in an OpenAI Chat request.
pub fn add_tool(chat: &mut Value) {
    let tool = json!({
        "type": "function",
        "function": {
            "name": TOOL_NAME,
            "description": "Search the web for current information. Returns result titles, URLs, and snippets. \
                            Use it when the answer depends on recent or external information.",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string", "description": "The search query"}},
                "required": ["query"],
            },
        }
    });
    match chat["tools"].as_array_mut() {
        Some(tools) => tools.push(tool),
        None => chat["tools"] = json!([tool]),
    }
}

/// Opens the follow-up request after a round of searches.
pub type Reopen = Arc<
    dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<EventStream, UpstreamError>> + Send>>
        + Send
        + Sync,
>;

pub struct Searcher {
    pub http: reqwest::Client,
    pub config: WebSearchConfig,
}

/// Run the turn: pass the model's output through, and whenever it calls
/// [`TOOL_NAME`], search, report it, and continue the conversation.
/// `chat` is the request that produced `first`.
pub fn run(
    first: impl Stream<Item = Event> + Send + 'static,
    chat: Value,
    plan: Plan,
    searcher: Searcher,
    reasoning_history: bool,
    reopen: Reopen,
) -> impl Stream<Item = Event> + Send + 'static {
    async_stream::stream! {
        let mut events: EventStream = Box::pin(first);
        let mut chat = chat;
        let mut total = Usage::default();
        let mut used = 0u32;
        for round in 1..=MAX_ROUNDS {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            let mut finish = None;
            while let Some(event) = events.next().await {
                match event {
                    Event::Text(delta) => {
                        text.push_str(&delta);
                        yield Event::Text(delta);
                    }
                    Event::Reasoning(delta) => {
                        reasoning.push_str(&delta);
                        yield Event::Reasoning(delta);
                    }
                    Event::ToolCall { id, name, arguments } => calls.push((id, name, arguments)),
                    Event::Usage(usage) => add(&mut total, usage),
                    Event::Finish(reason) => {
                        finish = Some(reason);
                        break;
                    }
                    Event::Error(message) => {
                        yield Event::Usage(total);
                        yield Event::Error(message);
                        return;
                    }
                    other @ Event::WebSearch(_) => yield other,
                }
            }
            let searching = calls.iter().any(|(_, name, _)| name == TOOL_NAME);
            if !searching || round == MAX_ROUNDS {
                let mut client_calls = false;
                for (id, name, arguments) in calls {
                    if name != TOOL_NAME {
                        client_calls = true;
                        yield Event::ToolCall { id, name, arguments };
                    }
                }
                yield Event::Usage(total);
                yield Event::Finish(match finish {
                    Some(Finish::ToolCalls) if !client_calls => Finish::Stop,
                    Some(reason) => reason,
                    None => Finish::Stop,
                });
                return;
            }

            let mut assistant = json!({
                "role": "assistant",
                "content": text,
                "tool_calls": calls.iter().map(|(id, name, arguments)| json!({
                    "id": id, "type": "function",
                    "function": {"name": name, "arguments": arguments},
                })).collect::<Vec<_>>(),
            });
            if reasoning_history && !reasoning.is_empty() {
                assistant["reasoning_content"] = json!(reasoning);
            }
            let mut replies = vec![assistant];
            for (id, name, arguments) in &calls {
                let content = if name == TOOL_NAME {
                    let query = query_of(arguments);
                    let outcome = match (&query, plan.max_uses) {
                        (None, _) => Err(Failure {
                            code: "invalid_tool_input",
                            message: "web_search needs a non-empty query".into(),
                        }),
                        (Some(_), Some(max)) if used >= max => Err(Failure {
                            code: "max_uses_exceeded",
                            message: format!("the search limit of {max} was reached"),
                        }),
                        (Some(query), _) => {
                            used += 1;
                            searcher.search(query, &plan).await
                        }
                    };
                    let search = Search {
                        id: new_id("srvtoolu_"),
                        query: query.unwrap_or_default(),
                        outcome,
                    };
                    let content = describe(&search);
                    yield Event::WebSearch(search);
                    content
                } else {
                    "Not run: call this tool again after reading the search results.".to_string()
                };
                replies.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
            }
            if let Some(messages) = chat["messages"].as_array_mut() {
                messages.extend(replies);
            }
            events = match reopen(chat.clone()).await {
                Ok(events) => events,
                Err(err) => {
                    yield Event::Usage(total);
                    yield Event::Error(err.to_string());
                    return;
                }
            };
        }
    }
}

fn add(total: &mut Usage, usage: Usage) {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.cached_tokens += usage.cached_tokens;
    total.reasoning_tokens += usage.reasoning_tokens;
}

fn query_of(arguments: &str) -> Option<String> {
    let args: Value = serde_json::from_str(arguments).ok()?;
    let query = args["query"].as_str()?.trim();
    (!query.is_empty()).then(|| query.to_string())
}

/// The search result as the model reads it.
pub fn describe(search: &Search) -> String {
    match &search.outcome {
        Ok(hits) if hits.is_empty() => format!("No web results for \"{}\".", search.query),
        Ok(hits) => {
            let mut out = format!("Web results for \"{}\":\n", search.query);
            for (n, hit) in hits.iter().enumerate() {
                out.push_str(&format!("\n{}. {}\n   {}\n", n + 1, hit.title, hit.url));
                if !hit.snippet.is_empty() {
                    out.push_str(&format!("   {}\n", hit.snippet));
                }
                if let Some(age) = &hit.age {
                    out.push_str(&format!("   ({age})\n"));
                }
            }
            out
        }
        Err(failure) => format!(
            "Web search for \"{}\" failed: {}",
            search.query, failure.message
        ),
    }
}

impl Searcher {
    pub async fn search(&self, query: &str, plan: &Plan) -> Result<Vec<Hit>, Failure> {
        let unavailable = |message: String| Failure {
            code: "unavailable",
            message,
        };
        let config = &self.config;
        let url = |default: &str| {
            if config.url.trim().is_empty() {
                default.to_string()
            } else {
                config.url.trim().to_string()
            }
        };
        let request = match config.backend {
            SearchBackend::Duckduckgo => self
                .http
                .get(url(DUCKDUCKGO_URL))
                .query(&[("q", query)])
                .header("user-agent", USER_AGENT),
            SearchBackend::Brave => {
                let key = config.resolved_api_key();
                if key.is_empty() {
                    return Err(unavailable("the Brave backend needs an API key".into()));
                }
                self.http
                    .get(url(BRAVE_URL))
                    .query(&[("q", query), ("count", &config.max_results.to_string())])
                    .header("x-subscription-token", key)
                    .header("accept", "application/json")
            }
            SearchBackend::Searxng => self
                .http
                .get(format!("{}/search", url("").trim_end_matches('/')))
                .query(&[("q", query), ("format", "json")]),
        };
        let response = request
            .timeout(SEARCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| unavailable(format!("search request failed: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| unavailable(format!("search response failed: {e}")))?;
        if !status.is_success() {
            return Err(unavailable(format!(
                "search backend returned HTTP {status}"
            )));
        }
        let hits = match config.backend {
            SearchBackend::Duckduckgo => parse_duckduckgo(&body),
            SearchBackend::Brave => parse_json(&body, "/web/results", "description", "age"),
            SearchBackend::Searxng => parse_json(&body, "/results", "content", "publishedDate"),
        }
        .map_err(unavailable)?;
        Ok(hits
            .into_iter()
            .filter(|hit| allowed(&hit.url, plan))
            .take(config.max_results.max(1))
            .collect())
    }
}

fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

fn matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn allowed(url: &str, plan: &Plan) -> bool {
    let Some(host) = host(url) else {
        return false;
    };
    if plan
        .blocked_domains
        .iter()
        .any(|d| matches_domain(&host, d))
    {
        return false;
    }
    plan.allowed_domains.is_empty()
        || plan
            .allowed_domains
            .iter()
            .any(|d| matches_domain(&host, d))
}

/// Brave (`web.results[]`) and SearXNG (`results[]`) answers.
fn parse_json(body: &str, list: &str, snippet: &str, age: &str) -> Result<Vec<Hit>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("unexpected search response: {e}"))?;
    Ok(value
        .pointer(list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let url = item["url"].as_str()?.to_string();
            Some(Hit {
                title: strip_tags(item["title"].as_str().unwrap_or(&url)),
                snippet: strip_tags(item[snippet].as_str().unwrap_or("")),
                age: item[age]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .map(str::to_string),
                url,
            })
        })
        .collect())
}

/// Results of DuckDuckGo's lite HTML page: result links carry the target
/// in a `uddg` parameter, and a `result-snippet` cell follows each.
pub fn parse_duckduckgo(html: &str) -> Result<Vec<Hit>, String> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<a ") {
        rest = &rest[start..];
        let Some(tag_end) = rest.find('>') else { break };
        let tag = &rest[..tag_end];
        let after = &rest[tag_end + 1..];
        let Some(close) = after.find("</a>") else {
            break;
        };
        let title = strip_tags(&after[..close]);
        rest = &after[close..];
        let Some(href) = attribute(tag, "href") else {
            continue;
        };
        let href = decode_entities(&href);
        let target = match href.split_once("uddg=") {
            Some((_, encoded)) => percent_decode(encoded.split('&').next().unwrap_or("")),
            None if tag.contains("result-link") && href.starts_with("http") => href.clone(),
            None => continue,
        };
        if !target.starts_with("http") || target.contains("duckduckgo.com/y.js") {
            continue;
        }
        let next_link = rest.find("uddg=").unwrap_or(rest.len());
        let snippet = rest[..next_link]
            .find("result-snippet")
            .and_then(|at| {
                let cell = &rest[at..next_link];
                let open = cell.find('>')? + 1;
                let end = cell.find("</td>").unwrap_or(cell.len());
                (open <= end).then(|| strip_tags(&cell[open..end]))
            })
            .unwrap_or_default();
        if hits.iter().any(|h| h.url == target) {
            continue;
        }
        hits.push(Hit {
            title: if title.is_empty() {
                target.clone()
            } else {
                title
            },
            url: target,
            snippet,
            age: None,
        });
    }
    if hits.is_empty() && html.contains("anomaly") {
        return Err("DuckDuckGo refused the search (rate limited); try again later or configure another backend".into());
    }
    Ok(hits)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let value = &tag[at..];
    let quote = value.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &value[1..];
        Some(inner[..inner.find(quote)?].to_string())
    } else {
        Some(value.split([' ', '>']).next()?.to_string())
    }
}

fn strip_tags(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit() =>
            {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LITE: &str = r#"<html><body><table>
      <tr><td>1.&nbsp;</td><td>
        <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2Ftokio%2Ftutorial%2Fselect&amp;rut=abc" class='result-link'>Select | <b>Tokio</b> - An asynchronous Rust runtime</a>
      </td></tr>
      <tr><td></td><td class='result-snippet'>
        The <b>tokio::select!</b> macro allows waiting on multiple async computations &amp; returns when a single one completes.
      </td></tr>
      <tr><td>2.&nbsp;</td><td>
        <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio%2Flatest%2Ftokio%2Fmacro.select.html&amp;rut=def" class='result-link'>select in tokio - Rust</a>
      </td></tr>
      <tr><td></td><td class='result-snippet'>Waits on multiple concurrent branches.</td></tr>
      <tr><td><a href="https://duckduckgo.com/y.js?ad_domain=x&amp;uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad">Ad</a></td></tr>
      <a href="/lite/?q=next">Next Page</a>
    </table></body></html>"#;

    #[test]
    fn parses_duckduckgo_lite_results() {
        let hits = parse_duckduckgo(LITE).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://tokio.rs/tokio/tutorial/select");
        assert_eq!(
            hits[0].title,
            "Select | Tokio - An asynchronous Rust runtime"
        );
        assert_eq!(
            hits[0].snippet,
            "The tokio::select! macro allows waiting on multiple async computations & returns when a single one completes."
        );
        assert_eq!(
            hits[1].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert_eq!(hits[1].snippet, "Waits on multiple concurrent branches.");
        assert!(parse_duckduckgo("<html>anomaly detected</html>").is_err());
        assert_eq!(parse_duckduckgo("<html>nothing</html>").unwrap(), vec![]);
    }

    #[test]
    fn parses_json_backends() {
        let brave = r#"{"web": {"results": [{"title": "<strong>Tokio</strong>", "url": "https://tokio.rs/", "description": "A runtime", "age": "2 days ago"}]}}"#;
        let hits = parse_json(brave, "/web/results", "description", "age").unwrap();
        assert_eq!(hits[0].title, "Tokio");
        assert_eq!(hits[0].age.as_deref(), Some("2 days ago"));
        let searx =
            r#"{"results": [{"title": "T", "url": "https://a.example/x", "content": "c"}]}"#;
        let hits = parse_json(searx, "/results", "content", "publishedDate").unwrap();
        assert_eq!(hits[0].snippet, "c");
        assert!(parse_json("not json", "/results", "content", "x").is_err());
    }

    #[test]
    fn filters_domains() {
        let plan = Plan {
            allowed_domains: vec!["tokio.rs".into()],
            ..Plan::default()
        };
        assert!(allowed("https://tokio.rs/x", &plan));
        assert!(allowed("https://docs.tokio.rs/x", &plan));
        assert!(!allowed("https://nottokio.rs/x", &plan));
        let plan = Plan {
            blocked_domains: vec!["example.com".into()],
            ..Plan::default()
        };
        assert!(!allowed("https://www.example.com:8443/a", &plan));
        assert!(allowed("https://example.org/", &plan));
    }

    #[test]
    fn decodes_text() {
        assert_eq!(percent_decode("a%20b%2Fc+d%zz"), "a b/c d%zz");
        assert_eq!(
            decode_entities("&lt;a&gt; &#39;x&#x27; & y &unknown;"),
            "<a> 'x' & y &unknown;"
        );
    }

    #[test]
    fn detects_client_search_tools() {
        let claude = json!({"tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8,
                                        "allowed_domains": ["Docs.rs"]}]});
        let plan = messages_plan(&claude).unwrap();
        assert_eq!(plan.max_uses, Some(8));
        assert_eq!(plan.allowed_domains, ["docs.rs"]);
        assert!(messages_plan(&json!({"tools": [{"name": "Bash"}]})).is_none());

        let codex = json!({"tools": [{"type": "function", "name": "exec_command"}, {"type": "web_search", "external_web_access": false}]});
        assert_eq!(responses_plan(&codex), Some(Plan::default()));
        let own =
            json!({"tools": [{"type": "function", "name": "web_search"}, {"type": "web_search"}]});
        assert!(responses_plan(&own).is_none());
    }

    #[test]
    fn describes_results_for_the_model() {
        let search = Search {
            id: "s".into(),
            query: "q".into(),
            outcome: Ok(vec![Hit {
                title: "T".into(),
                url: "https://a/".into(),
                snippet: "S".into(),
                age: Some("1 day ago".into()),
            }]),
        };
        assert_eq!(
            describe(&search),
            "Web results for \"q\":\n\n1. T\n   https://a/\n   S\n   (1 day ago)\n"
        );
    }
}
