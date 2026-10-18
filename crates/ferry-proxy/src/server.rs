//! The local gateway: `/v1/messages` for Claude Code, `/v1/responses` for Codex.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};

use crate::anthropic::{self, AnthropicEncoder};
use crate::config::{Config, ProviderKind};
use crate::recovery::{continuation_request, OverlapTrimmer, MAX_RECOVERIES};
use crate::responses::{self, ResponsesEncoder};
use crate::router::{self, Target};
use crate::stream::{Event, Usage};
use crate::upstream::{self, UpstreamError};

const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const LOG_CAPACITY: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct RequestRecord {
    pub started_at_ms: u64,
    pub api: &'static str,
    pub requested_model: String,
    pub routed_model: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

struct Inner {
    config: RwLock<Arc<Config>>,
    config_path: Option<PathBuf>,
    config_mtime: Mutex<Option<SystemTime>>,
    http: reqwest::Client,
    log: Mutex<VecDeque<RequestRecord>>,
}

/// Shared gateway state. Cheap to clone.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    /// `config_path` enables reloading when another process (the desktop app
    /// or a text editor) rewrites the file.
    pub fn new(config: Config, config_path: Option<PathBuf>) -> Self {
        let mtime = config_path.as_ref().and_then(|p| modified(p));
        Self(Arc::new(Inner {
            config: RwLock::new(Arc::new(config)),
            config_path,
            config_mtime: Mutex::new(mtime),
            http: upstream::build_client(),
            log: Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)),
        }))
    }

    pub fn config(&self) -> Arc<Config> {
        self.reload_if_changed();
        self.0.config.read().unwrap().clone()
    }

    pub fn set_config(&self, config: Config) {
        *self.0.config.write().unwrap() = Arc::new(config);
        if let Some(path) = &self.0.config_path {
            *self.0.config_mtime.lock().unwrap() = modified(path);
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.0.http
    }

    pub fn recent_requests(&self) -> Vec<RequestRecord> {
        self.0.log.lock().unwrap().iter().rev().cloned().collect()
    }

    fn record(&self, record: RequestRecord) {
        let mut log = self.0.log.lock().unwrap();
        if log.len() == LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(record);
    }

    fn reload_if_changed(&self) {
        let Some(path) = &self.0.config_path else {
            return;
        };
        let current = modified(path);
        let mut known = self.0.config_mtime.lock().unwrap();
        if current.is_none() || current == *known {
            return;
        }
        *known = current;
        match Config::load(path) {
            Ok(config) => {
                tracing::info!(path = %path.display(), "configuration reloaded");
                *self.0.config.write().unwrap() = Arc::new(config);
            }
            Err(err) => tracing::error!("keeping previous configuration: {err:#}"),
        }
    }
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn app(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/models", get(models))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/", get(health))
        .route("/health", get(health))
        .merge(api)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Serve until `shutdown` resolves.
pub async fn serve(
    state: AppState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "ferry", "version": env!("CARGO_PKG_VERSION")}))
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let responses_api = request.uri().path().starts_with("/v1/responses");
    let deny = |status: StatusCode, message: &str| {
        let body = if responses_api {
            json!({"error": {"type": "invalid_request_error", "code": "invalid_api_key", "message": message}})
        } else {
            anthropic::error_body("authentication_error", message)
        };
        (status, Json(body)).into_response()
    };
    // Browsers always send Origin on cross-site requests; CLI clients never
    // do. Refusing them keeps web pages from spending the user's quota.
    if request.headers().contains_key("origin") {
        return deny(StatusCode::FORBIDDEN, "browser requests are not allowed");
    }
    let token = state.config().server.auth_token.clone();
    if token.is_empty() || presented_token(request.headers()).as_deref() == Some(token.as_str()) {
        return next.run(request).await;
    }
    deny(StatusCode::UNAUTHORIZED, "invalid Ferry token")
}

fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.trim().to_string());
    }
    let auth = headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = auth.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
}

fn parse_body(bytes: &Bytes) -> Result<Value, String> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err("request body must be a JSON object".into()),
        Err(err) => Err(format!("invalid JSON body: {err}")),
    }
}

fn anthropic_error(status: u16, message: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (
        code,
        Json(anthropic::error_body(
            anthropic::error_type_for_status(status),
            message,
        )),
    )
        .into_response()
}

fn responses_error(status: u16, message: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let kind = if status == 429 {
        "rate_limit_exceeded"
    } else if status < 500 {
        "invalid_request_error"
    } else {
        "server_error"
    };
    (
        code,
        Json(json!({"error": {"type": kind, "code": kind, "message": message}})),
    )
        .into_response()
}

async fn count_tokens(body: Bytes) -> Response {
    match parse_body(&body) {
        Ok(body) => {
            Json(json!({"input_tokens": anthropic::estimate_input_tokens(&body)})).into_response()
        }
        Err(message) => anthropic_error(400, &message),
    }
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    let config = state.config();
    let data: Vec<Value> = router::advertised_models(&config)
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "type": "model",
                "object": "model",
                "display_name": id,
                "created_at": "2025-01-01T00:00:00Z",
                "created": 1735689600,
                "owned_by": "ferry",
            })
        })
        .collect();
    let first = data.first().map(|m| m["id"].clone());
    let last = data.last().map(|m| m["id"].clone());
    Json(
        json!({"object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last}),
    )
}

/// Writes a [`RequestRecord`] when dropped, including on client disconnect.
struct Recorder {
    state: AppState,
    record: RequestRecord,
    started: Instant,
    done: bool,
    /// Targets a broken stream was resumed on, filled in by [`resumable_events`].
    resumed_on: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn new(state: &AppState, api: &'static str, requested: &str) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            state: state.clone(),
            record: RequestRecord {
                started_at_ms: now,
                api,
                requested_model: requested.to_string(),
                routed_model: String::new(),
                outcome: "cancelled".into(),
                duration_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
            },
            started: Instant::now(),
            done: false,
            resumed_on: Arc::default(),
        }
    }

    fn observe(&mut self, event: &Event) {
        match event {
            Event::Usage(usage) => self.usage(*usage),
            Event::Finish(_) => {
                let resumed = self.resumed_on.lock().unwrap().clone();
                if resumed.is_empty() {
                    self.finish("ok".into());
                } else {
                    self.finish(format!("ok (resumed on {})", resumed.join(", ")));
                }
            }
            Event::Error(message) => self.finish(format!("error: {message}")),
            _ => {}
        }
    }

    fn usage(&mut self, usage: Usage) {
        self.record.input_tokens = usage.input_tokens;
        self.record.output_tokens = usage.output_tokens;
    }

    fn finish(&mut self, outcome: String) {
        if !self.done {
            self.record.outcome = outcome;
        }
        self.done = true;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.record.duration_ms = self.started.elapsed().as_millis() as u64;
        self.state.record(self.record.clone());
    }
}

struct Attempt<'a> {
    state: &'a AppState,
    config: &'a Config,
    headers: &'a HeaderMap,
}

impl Attempt<'_> {
    async fn open(
        &self,
        target: &Target,
        body: &Value,
    ) -> Result<reqwest::Response, UpstreamError> {
        let timeout = Duration::from_secs(self.config.server.connect_timeout_secs.max(5));
        upstream::send(
            self.state.http(),
            &target.provider,
            body,
            Some(self.headers),
            timeout,
            self.config.server.max_retries,
        )
        .await
    }
}

/// Builds the OpenAI Chat request for a target, or `None` if the target
/// cannot serve this kind of request.
type BuildRequest = Arc<dyn Fn(&Target) -> Option<Value> + Send + Sync>;

/// Everything needed to reopen a request after its stream broke.
struct Resume {
    state: AppState,
    config: Arc<Config>,
    headers: HeaderMap,
    targets: Vec<Target>,
    build: BuildRequest,
}

/// Events from `first` (opened on `targets[index]`). If the stream breaks,
/// the request is reopened (same target first, then fallbacks) and the
/// answer continues from what the client already received.
fn resumable_events(
    resume: Resume,
    mut index: usize,
    first: reqwest::Response,
    resumed_on: Arc<Mutex<Vec<String>>>,
) -> impl Stream<Item = Event> + Send + 'static {
    async_stream::stream! {
        let mut response = first;
        let mut delivered = String::new();
        let mut usage = Usage::default();
        let mut recoveries = 0;
        let mut trimmer: Option<OverlapTrimmer> = None;
        loop {
            let mut failure = None;
            {
                let events = upstream::chat_events(response);
                let mut events = std::pin::pin!(events);
                while let Some(event) = events.next().await {
                    match event {
                        Event::Text(text) => {
                            let text = match &mut trimmer {
                                Some(trimmer) => trimmer.push(&text),
                                None => text,
                            };
                            if !text.is_empty() {
                                delivered.push_str(&text);
                                yield Event::Text(text);
                            }
                        }
                        // A resumed attempt re-thinks from scratch; once the
                        // answer has started that would be a stray block mid-answer.
                        Event::Reasoning(_) if trimmer.is_some() => {}
                        Event::Reasoning(text) => yield Event::Reasoning(text),
                        Event::Usage(more) => {
                            usage.input_tokens += more.input_tokens;
                            usage.output_tokens += more.output_tokens;
                            usage.cached_tokens += more.cached_tokens;
                            usage.reasoning_tokens += more.reasoning_tokens;
                        }
                        Event::Error(message) => {
                            failure = Some(message);
                            break;
                        }
                        other => {
                            if let Some(rest) = trimmer.as_mut().map(OverlapTrimmer::finish) {
                                if !rest.is_empty() {
                                    delivered.push_str(&rest);
                                    yield Event::Text(rest);
                                }
                            }
                            let finished = matches!(other, Event::Finish(_));
                            if finished && usage != Usage::default() {
                                yield Event::Usage(usage);
                            }
                            yield other;
                            if finished {
                                return;
                            }
                        }
                    }
                }
            }
            let message = failure.unwrap_or_else(|| "provider stream ended unexpectedly".into());
            if recoveries >= MAX_RECOVERIES {
                yield Event::Error(message);
                return;
            }
            recoveries += 1;
            tracing::warn!(
                target = %resume.targets[index].reference(),
                delivered_chars = delivered.chars().count(),
                error = %message,
                "stream broke; resuming"
            );
            let timeout = Duration::from_secs(resume.config.server.connect_timeout_secs.max(5));
            let mut reopened = None;
            let mut last_error = None;
            for (i, target) in resume.targets.iter().enumerate().skip(index) {
                let Some(mut request) = (resume.build)(target) else { continue };
                if !delivered.is_empty() {
                    request = continuation_request(request, &delivered);
                }
                match upstream::send(
                    resume.state.http(),
                    &target.provider,
                    &request,
                    Some(&resume.headers),
                    timeout,
                    resume.config.server.max_retries,
                )
                .await
                {
                    Ok(next) => {
                        reopened = Some((i, next));
                        break;
                    }
                    Err(err) => last_error = Some(err),
                }
            }
            let Some((i, next)) = reopened else {
                let reason = last_error.map(|e| e.to_string()).unwrap_or_else(|| "no provider left".into());
                yield Event::Error(format!("{message} (resume failed: {reason})"));
                return;
            };
            index = i;
            response = next;
            resumed_on.lock().unwrap().push(resume.targets[i].reference());
            trimmer = (!delivered.is_empty()).then(|| OverlapTrimmer::new(&delivered));
        }
    }
}

fn sse_response<S>(frames: S) -> Response
where
    S: Stream<Item = (&'static str, Value)> + Send + 'static,
{
    let events = frames.map(|(name, data)| {
        Ok::<_, Infallible>(SseEvent::default().event(name).data(data.to_string()))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse_body(&body) {
        Ok(body) => body,
        Err(message) => return anthropic_error(400, &message),
    };
    let config = state.config();
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wants_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut recorder = Recorder::new(&state, "messages", &requested);
    let targets = match router::resolve(&config, &requested) {
        Ok(targets) => targets,
        Err(err) => {
            recorder.finish(format!("error: {err}"));
            return anthropic_error(400, &err.to_string());
        }
    };
    let attempt = Attempt {
        state: &state,
        config: &config,
        headers: &headers,
    };
    let mut last_error = None;
    for (index, target) in targets.iter().enumerate() {
        recorder.record.routed_model = target.reference();
        match target.provider.kind {
            ProviderKind::Anthropic => {
                let mut native = body.clone();
                native["model"] = json!(target.model);
                if let (Some(cap), Some(max)) = (
                    target.provider.max_tokens_cap,
                    native["max_tokens"].as_u64(),
                ) {
                    native["max_tokens"] = json!(max.min(cap as u64));
                }
                match attempt.open(target, &native).await {
                    Ok(response) => {
                        recorder.finish("ok (passthrough)".into());
                        return passthrough(response, recorder);
                    }
                    Err(err) => last_error = Some(err),
                }
            }
            ProviderKind::OpenaiChat => {
                let chat = anthropic::to_chat_request(&body, &target.model, &target.provider);
                match attempt.open(target, &chat).await {
                    Ok(response) => {
                        let estimate = anthropic::estimate_input_tokens(&body);
                        let mut encoder = AnthropicEncoder::new(&requested, estimate);
                        let source = body.clone();
                        let resume = Resume {
                            state: state.clone(),
                            config: config.clone(),
                            headers: headers.clone(),
                            targets: targets.clone(),
                            build: Arc::new(move |t: &Target| {
                                (t.provider.kind == ProviderKind::OpenaiChat).then(|| {
                                    anthropic::to_chat_request(&source, &t.model, &t.provider)
                                })
                            }),
                        };
                        let events =
                            resumable_events(resume, index, response, recorder.resumed_on.clone());
                        if wants_stream {
                            let frames = async_stream::stream! {
                                let mut recorder = recorder;
                                let mut events = std::pin::pin!(events);
                                for frame in encoder.start() {
                                    yield frame;
                                }
                                while let Some(event) = events.next().await {
                                    recorder.observe(&event);
                                    for frame in encoder.push(event) {
                                        yield frame;
                                    }
                                    if encoder.is_finished() {
                                        break;
                                    }
                                }
                            };
                            return sse_response(frames);
                        }
                        let mut frames = encoder.start();
                        let mut events = std::pin::pin!(events);
                        while let Some(event) = events.next().await {
                            recorder.observe(&event);
                            frames.extend(encoder.push(event));
                        }
                        return match anthropic::aggregate(&frames) {
                            Ok(message) => Json(message).into_response(),
                            Err(error) => (StatusCode::BAD_GATEWAY, Json(error)).into_response(),
                        };
                    }
                    Err(err) => last_error = Some(err),
                }
            }
        }
        if let Some(err) = &last_error {
            tracing::warn!(target = %target.reference(), error = %err, "candidate failed");
        }
    }
    let err = last_error.unwrap_or(UpstreamError {
        status: 502,
        message: "no provider available".into(),
    });
    recorder.finish(format!("error: {err}"));
    anthropic_error(err.status, &err.message)
}

fn passthrough(response: reqwest::Response, recorder: Recorder) -> Response {
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = response.bytes_stream();
    let body = async_stream::stream! {
        let _recorder = recorder;
        let mut bytes = std::pin::pin!(bytes);
        while let Some(chunk) = bytes.next().await {
            yield chunk.map_err(std::io::Error::other);
        }
    };
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", "no-cache")
        .body(Body::from_stream(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

async fn responses_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = match parse_body(&body) {
        Ok(body) => body,
        Err(message) => return responses_error(400, &message),
    };
    let config = state.config();
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wants_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut recorder = Recorder::new(&state, "responses", &requested);
    let targets = match router::resolve(&config, &requested) {
        Ok(targets) => targets,
        Err(err) => {
            recorder.finish(format!("error: {err}"));
            return responses_error(400, &err.to_string());
        }
    };
    let attempt = Attempt {
        state: &state,
        config: &config,
        headers: &headers,
    };
    let mut last_error = None;
    for (index, target) in targets.iter().enumerate() {
        recorder.record.routed_model = target.reference();
        if target.provider.kind != ProviderKind::OpenaiChat {
            last_error = Some(UpstreamError {
                status: 400,
                message: format!(
                    "{}: Anthropic-type providers cannot serve Codex yet",
                    target.provider.id
                ),
            });
            continue;
        }
        let conversion = responses::to_chat_request(&body, &target.model, &target.provider);
        match attempt.open(target, &conversion.request).await {
            Ok(response) => {
                let mut encoder = ResponsesEncoder::new(&requested, conversion.custom_tools);
                let source = body.clone();
                let resume = Resume {
                    state: state.clone(),
                    config: config.clone(),
                    headers: headers.clone(),
                    targets: targets.clone(),
                    build: Arc::new(move |t: &Target| {
                        (t.provider.kind == ProviderKind::OpenaiChat).then(|| {
                            responses::to_chat_request(&source, &t.model, &t.provider).request
                        })
                    }),
                };
                let events = resumable_events(resume, index, response, recorder.resumed_on.clone());
                if wants_stream {
                    let frames = async_stream::stream! {
                        let mut recorder = recorder;
                        let mut events = std::pin::pin!(events);
                        for frame in encoder.start() {
                            yield frame;
                        }
                        while let Some(event) = events.next().await {
                            recorder.observe(&event);
                            for frame in encoder.push(event) {
                                yield frame;
                            }
                            if encoder.is_finished() {
                                break;
                            }
                        }
                    };
                    return sse_response(frames);
                }
                let mut last = None;
                let mut events = std::pin::pin!(events);
                while let Some(event) = events.next().await {
                    recorder.observe(&event);
                    for (name, data) in encoder.push(event) {
                        if name == "response.completed" || name == "response.failed" {
                            last = Some(data["response"].clone());
                        }
                    }
                }
                return match last {
                    Some(response) if response["status"] == "completed" => {
                        Json(response).into_response()
                    }
                    Some(response) => (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({"error": response["error"]})),
                    )
                        .into_response(),
                    None => responses_error(502, "provider stream ended unexpectedly"),
                };
            }
            Err(err) => {
                tracing::warn!(target = %target.reference(), error = %err, "candidate failed");
                last_error = Some(err);
            }
        }
    }
    let err = last_error.unwrap_or(UpstreamError {
        status: 502,
        message: "no provider available".into(),
    });
    recorder.finish(format!("error: {err}"));
    responses_error(err.status, &err.message)
}
