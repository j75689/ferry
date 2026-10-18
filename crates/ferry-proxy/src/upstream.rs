//! Upstream HTTP calls with retry, plus stream decoding into neutral events.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::Value;

use crate::config::{ProviderConfig, ProviderKind};
use crate::stream::{error_message, ChatStreamDecoder, Event, SseParser};

/// Neutral events from any provider, ending with `Finish` or `Error`.
pub type EventStream = std::pin::Pin<Box<dyn Stream<Item = Event> + Send>>;

/// Longest silence tolerated while a stream is open.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct UpstreamError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.message)
    }
}

pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("ferry/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client configuration is valid")
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn endpoint(provider: &ProviderConfig) -> String {
    let base = provider.trimmed_base_url();
    match provider.kind {
        ProviderKind::OpenaiChat | ProviderKind::Antigravity => format!("{base}/chat/completions"),
        ProviderKind::Anthropic if base.ends_with("/v1") => format!("{base}/messages"),
        ProviderKind::Anthropic => format!("{base}/v1/messages"),
    }
}

pub fn models_endpoint(provider: &ProviderConfig) -> String {
    let base = provider.trimmed_base_url();
    match provider.kind {
        ProviderKind::OpenaiChat | ProviderKind::Antigravity => format!("{base}/models"),
        ProviderKind::Anthropic if base.ends_with("/v1") => format!("{base}/models"),
        ProviderKind::Anthropic => format!("{base}/v1/models"),
    }
}

pub fn auth_headers(provider: &ProviderConfig, client_headers: Option<&HeaderMap>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let key = provider.resolved_api_key();
    match provider.kind {
        ProviderKind::OpenaiChat | ProviderKind::Antigravity => {
            if !key.is_empty() {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {key}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, value);
                }
            }
        }
        ProviderKind::Anthropic => {
            if let Ok(value) = HeaderValue::from_str(&key) {
                headers.insert("x-api-key", value);
            }
            let version = client_headers
                .and_then(|h| h.get("anthropic-version").cloned())
                .unwrap_or_else(|| HeaderValue::from_static("2023-06-01"));
            headers.insert("anthropic-version", version);
            if let Some(beta) = client_headers.and_then(|h| h.get("anthropic-beta")) {
                headers.insert("anthropic-beta", beta.clone());
            }
        }
    }
    for (name, value) in &provider.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// POST `body` and return the response once it has a 2xx status.
pub async fn send(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    body: &Value,
    client_headers: Option<&HeaderMap>,
    first_byte_timeout: Duration,
    max_retries: u32,
) -> Result<reqwest::Response, UpstreamError> {
    let url = endpoint(provider);
    let headers = auth_headers(provider, client_headers);
    let mut attempt = 0;
    loop {
        let request = http
            .post(&url)
            .headers(headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .json(body);
        let failure = match tokio::time::timeout(first_byte_timeout, request.send()).await {
            Err(_) => UpstreamError {
                status: 504,
                message: format!(
                    "{} did not respond within {}s",
                    provider.id,
                    first_byte_timeout.as_secs()
                ),
            },
            Ok(Err(err)) => UpstreamError {
                status: 502,
                message: format!("{} request failed: {}", provider.id, error_chain(&err)),
            },
            Ok(Ok(response)) if response.status().is_success() => return Ok(response),
            Ok(Ok(response)) => {
                let status = response.status().as_u16();
                let retry_after = retry_after(response.headers());
                let text = response.text().await.unwrap_or_default();
                let failure = UpstreamError {
                    status,
                    message: format!("{}: {}", provider.id, upstream_error_text(&text)),
                };
                if !is_retryable(status) || attempt >= max_retries {
                    return Err(failure);
                }
                attempt += 1;
                let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                tracing::warn!(provider = %provider.id, status, attempt, ?delay, "retrying upstream");
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        if attempt >= max_retries {
            return Err(failure);
        }
        attempt += 1;
        tracing::warn!(provider = %provider.id, error = %failure, attempt, "retrying upstream");
        tokio::time::sleep(backoff(attempt)).await;
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(750 * 2u64.pow(attempt.saturating_sub(1).min(4)))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs_f64(seconds.max(0.0)).min(MAX_RETRY_AFTER))
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

pub fn upstream_error_text(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(error) = value.get("error") {
            return error_message(error);
        }
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            return message.to_string();
        }
    }
    let mut text: String = trimmed.chars().take(500).collect();
    if text.is_empty() {
        text = "empty error response".into();
    }
    text
}

/// Decode an OpenAI Chat SSE response into neutral events, ending with `Finish` or `Error`.
pub fn chat_events(response: reqwest::Response) -> impl Stream<Item = Event> + Send {
    async_stream::stream! {
        let mut body = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut decoder = ChatStreamDecoder::default();
        loop {
            let next = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()).await {
                Ok(next) => next,
                Err(_) => {
                    yield Event::Error(format!("provider sent nothing for {}s", STREAM_IDLE_TIMEOUT.as_secs()));
                    return;
                }
            };
            match next {
                Some(Ok(bytes)) => {
                    for frame in parser.push(&bytes) {
                        for event in decoder.push_frame(&frame) {
                            let stop = matches!(event, Event::Error(_));
                            yield event;
                            if stop {
                                return;
                            }
                        }
                    }
                }
                Some(Err(err)) => {
                    yield Event::Error(format!("provider stream failed: {}", error_chain(&err)));
                    return;
                }
                None => break,
            }
        }
        for frame in parser.finish() {
            for event in decoder.push_frame(&frame) {
                yield event;
            }
        }
        for event in decoder.end_of_body() {
            yield event;
        }
    }
}

/// Fetch model ids from a provider's `/models` endpoint.
pub async fn list_models(
    http: &reqwest::Client,
    provider: &ProviderConfig,
) -> Result<Vec<String>, UpstreamError> {
    let response = http
        .get(models_endpoint(provider))
        .headers(auth_headers(provider, None))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|err| UpstreamError {
            status: 502,
            message: error_chain(&err),
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(UpstreamError {
            status,
            message: upstream_error_text(&text),
        });
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| UpstreamError {
        status: 502,
        message: "model list is not JSON".into(),
    })?;
    let items = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
        })
        .map(|id| id.trim_start_matches("models/").to_string())
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}
