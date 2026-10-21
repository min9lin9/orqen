//! Native OpenCode Go forwarding (HTTP/SSE) with same-model key retry.
//!
//! | Local route                 | Upstream (`base_url` + path)  | Credential header |
//! | --------------------------- | ----------------------------- | ----------------- |
//! | `POST /v1/chat/completions` | `/openai/v1/chat/completions` | `Authorization`   |
//! | `POST /v1/messages`         | `/anthropic/v1/messages`      | `x-api-key`       |
//! | `POST /v1/responses`        | `/openai/v1/responses`        | `Authorization`   |
//! | `GET /v1/models`            | `/v1/models`                  | `Authorization`   |
//!
//! Request bodies are forwarded byte-for-byte; only the top-level `model`
//! string is read (for model-scoped cooldowns). Responses are relayed
//! chunk-by-chunk without parsing or re-encoding, so SSE bytes and order,
//! tool-call IDs/arguments/results and reasoning blocks are unchanged.
//! There is no protocol conversion and no fallback to another model or
//! provider.
//!
//! Commit boundary: a retry is considered only after upstream answered with
//! a pre-content rejection (401, 402, 429) and before Orqen has returned any
//! response head. It always reuses the same body, so the same model, on a
//! different eligible key. Once a response head is returned the attempt is
//! final: there is no retry after headers. Every attempt runs inside the
//! request's handler future; when the client goes away or the service shuts
//! down, that future and its upstream request are dropped, so no new attempt
//! can start.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::{RequestLimits, UpstreamConfig};
use crate::error::{ErrorShape, ProxyError};
use crate::pool::{KeyIndex, KeyStatus, Outcome, Pool, Selection, fingerprint};
use crate::retry_after;
use crate::session::{self, Session, SessionConfig};

const MAX_MODEL_LEN: usize = 256;

pub struct AppState {
    pub client: reqwest::Client,
    pub pool: Pool,
    pub upstream: UpstreamConfig,
    pub limits: RequestLimits,
    pub session: SessionConfig,
    pub shutdown: CancellationToken,
    pub started: Instant,
    pub ephemeral_sessions: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
enum Route {
    Chat,
    Messages,
    Responses,
    Models,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat_completions",
            Self::Messages => "messages",
            Self::Responses => "responses",
            Self::Models => "models",
        }
    }

    fn upstream_path(self) -> &'static str {
        match self {
            Self::Chat => "/openai/v1/chat/completions",
            Self::Messages => "/anthropic/v1/messages",
            Self::Responses => "/openai/v1/responses",
            Self::Models => "/v1/models",
        }
    }

    fn shape(self) -> ErrorShape {
        match self {
            Self::Messages => ErrorShape::Anthropic,
            Self::Chat | Self::Responses | Self::Models => ErrorShape::OpenAi,
        }
    }
}

/// Upstream client: bounded connect time, no redirects (credentials must
/// not follow a redirect) and every transparent decompression disabled, so
/// `Content-Encoding` and the encoded bytes reach the client exactly as
/// upstream sent them, whichever reqwest codecs the dependency graph
/// enables. Loopback upstreams bypass any environment HTTP proxy.
pub fn build_client(upstream: &UpstreamConfig) -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(upstream.connect_timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .user_agent(concat!("orqen/", env!("CARGO_PKG_VERSION")));
    let builder = if upstream.direct {
        builder.no_proxy()
    } else {
        builder
    };
    builder.build()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .with_state(state)
}

async fn chat(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    forward(&state, Route::Chat, &headers, Some(body)).await
}

async fn messages(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    forward(&state, Route::Messages, &headers, Some(body)).await
}

async fn responses(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    forward(&state, Route::Responses, &headers, Some(body)).await
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    forward(&state, Route::Models, &headers, None).await
}

#[derive(Serialize)]
struct Health<'a> {
    status: &'static str,
    version: &'static str,
    uptime_secs: u64,
    upstream: &'a str,
    ephemeral_sessions: u64,
    keys: Vec<KeyStatus>,
}

async fn health(State(state): State<Arc<AppState>>) -> Response {
    let draining = state.shutdown.is_cancelled();
    let body = Health {
        status: if draining { "draining" } else { "ok" },
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.started.elapsed().as_secs(),
        upstream: &state.upstream.base_url,
        ephemeral_sessions: state.ephemeral_sessions.load(Ordering::Relaxed),
        keys: state.pool.snapshot(Instant::now()),
    };
    let status = if draining {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (status, axum::Json(body)).into_response()
}

async fn forward(
    state: &AppState,
    route: Route,
    headers: &HeaderMap,
    body: Option<Body>,
) -> Response {
    match attempt_loop(state, route, headers, body).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                route = route.name(),
                status = error.status().as_u16(),
                %error,
                "request answered by gateway"
            );
            error.into_response(route.shape())
        }
    }
}

struct AttemptContext<'a> {
    route: Route,
    model: &'a str,
    key: KeyIndex,
    attempt: usize,
    session_source: &'a str,
    session_tag: &'a str,
}

async fn attempt_loop(
    state: &AppState,
    route: Route,
    headers: &HeaderMap,
    body: Option<Body>,
) -> Result<Response, ProxyError> {
    if state.pool.is_empty() {
        return Err(ProxyError::NoCredentials);
    }
    let mut payload = None;
    let mut model = String::new();
    let mut session = None;
    if let Some(body) = body {
        let resolved = session::resolve(headers, &state.session)?;
        let bytes = read_body(body, headers, &state.limits).await?;
        model = request_model(&bytes)?;
        if resolved.is_ephemeral() {
            state.ephemeral_sessions.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                route = route.name(),
                "no conversation identity header; minted an ephemeral per-request session"
            );
        }
        session = Some(resolved);
        payload = Some(bytes);
    }
    let affinity = session.as_ref().map_or("", Session::id);
    let session_source = session.as_ref().map_or("none", Session::source_label);
    let session_tag = session
        .as_ref()
        .map_or_else(|| "none".to_owned(), |session| fingerprint(session.id()));

    let mut tried: Vec<KeyIndex> = Vec::new();
    loop {
        if state.shutdown.is_cancelled() {
            return Err(ProxyError::ShuttingDown);
        }
        let key = match state.pool.select(&model, affinity, &tried, Instant::now()) {
            Selection::Key(key) => key,
            Selection::Exhausted { retry_after } => {
                return Err(ProxyError::Exhausted { model, retry_after });
            }
        };
        tried.push(key);
        let attempt = tried.len();
        let request = upstream_request(
            state,
            route,
            headers,
            key,
            session.as_ref(),
            payload.clone(),
        );
        let timeout = state.upstream.response_header_timeout;
        let response = match tokio::time::timeout(timeout, request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(
                    route = route.name(),
                    key = state.pool.key_id(key),
                    attempt,
                    %error,
                    "upstream request failed before a response head"
                );
                // Not retried: a transport failure is not key-specific and
                // the request may already have reached upstream.
                return Err(if error.is_connect() {
                    ProxyError::UpstreamConnect
                } else {
                    ProxyError::Upstream
                });
            }
            Err(_) => return Err(ProxyError::UpstreamTimeout(timeout)),
        };
        let status = response.status();
        let context = AttemptContext {
            route,
            model: &model,
            key,
            attempt,
            session_source,
            session_tag: &session_tag,
        };
        let Some(outcome) = rejection_outcome(status, response.headers()) else {
            return Ok(relay(state, &context, response, None));
        };
        let now = Instant::now();
        let cooldown = state.pool.report(key, &model, outcome, now);
        tracing::info!(
            route = route.name(),
            model = %model,
            key = state.pool.key_id(key),
            scope = state.pool.scope_id(key),
            status = status.as_u16(),
            attempt,
            cooldown_secs = cooldown.as_secs(),
            "upstream rejected request before content; cooldown recorded"
        );
        let may_retry = attempt < state.upstream.max_attempts
            && !state.shutdown.is_cancelled()
            && state.pool.has_candidate(&model, &tried, now);
        if may_retry {
            drop(response);
            continue;
        }
        let retry_after = state.pool.unavailable_for(&model, now);
        return Ok(relay(state, &context, response, retry_after));
    }
}

fn rejection_outcome(status: StatusCode, headers: &HeaderMap) -> Option<Outcome> {
    match status {
        StatusCode::UNAUTHORIZED => Some(Outcome::Unauthorized),
        StatusCode::PAYMENT_REQUIRED => Some(Outcome::QuotaExhausted {
            retry_after: retry_after::parse(headers, SystemTime::now()),
        }),
        StatusCode::TOO_MANY_REQUESTS => Some(Outcome::RateLimited {
            retry_after: retry_after::parse(headers, SystemTime::now()),
        }),
        _ => None,
    }
}

fn forwardable(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "content-type"
            | "accept"
            | "anthropic-version"
            | "anthropic-beta"
            | "openai-beta"
            | "user-agent"
    ) || (name.starts_with("x-opencode-") && name != "x-opencode-session")
}

fn upstream_request(
    state: &AppState,
    route: Route,
    incoming: &HeaderMap,
    key: KeyIndex,
    session: Option<&Session>,
    payload: Option<Bytes>,
) -> reqwest::RequestBuilder {
    let url = format!("{}{}", state.upstream.base_url, route.upstream_path());
    let mut headers = HeaderMap::new();
    for (name, value) in incoming {
        if forwardable(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    let credential = state.pool.credential(key);
    match route {
        Route::Messages => {
            headers.insert("x-api-key", credential.api_key().clone());
        }
        Route::Chat | Route::Responses | Route::Models => {
            headers.insert(AUTHORIZATION, credential.bearer().clone());
        }
    }
    if let Some(session) = session {
        headers.insert(session::forward_header(), session.header_value().clone());
    }
    match payload {
        Some(payload) => {
            if !headers.contains_key(CONTENT_TYPE) {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
            state.client.post(url).headers(headers).body(payload)
        }
        None => state.client.get(url).headers(headers),
    }
}

/// Hop-by-hop and framing headers. `Content-Encoding` is end-to-end and is
/// kept: the body is relayed as opaque encoded bytes, never decoded.
fn is_hop_or_framing(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "set-cookie"
    )
}

/// Commit the upstream response head and stream its body.
fn relay(
    state: &AppState,
    context: &AttemptContext<'_>,
    response: reqwest::Response,
    retry_after: Option<Duration>,
) -> Response {
    let status = response.status();
    let key_id: Arc<str> = Arc::from(state.pool.key_id(context.key));
    tracing::info!(
        route = context.route.name(),
        model = context.model,
        key = %key_id,
        scope = state.pool.scope_id(context.key),
        status = status.as_u16(),
        attempt = context.attempt,
        session_source = context.session_source,
        session = context.session_tag,
        "response committed to client"
    );
    let mut headers = HeaderMap::with_capacity(response.headers().len() + 3);
    for (name, value) in response.headers() {
        if !is_hop_or_framing(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    if let Some(retry_after) = retry_after {
        headers.remove("retry-after-ms");
        headers.insert(RETRY_AFTER, retry_after::header_value(retry_after));
    }
    if let Ok(value) = HeaderValue::from_str(&key_id) {
        headers.insert("x-orqen-key", value);
    }
    headers.insert("x-orqen-attempts", HeaderValue::from(context.attempt));
    let stream = relay_stream(
        response,
        state.upstream.stream_idle_timeout,
        context.route,
        key_id,
    );
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

#[derive(Debug, thiserror::Error)]
enum RelayError {
    #[error("upstream body failed: {0}")]
    Upstream(reqwest::Error),
    #[error("upstream stream idle for more than {0:?}")]
    Idle(Duration),
}

/// One upstream chunk in flight at a time: the client's write backpressure
/// paces upstream reads, and nothing beyond the current chunk is buffered.
/// An upstream failure or idle timeout aborts the client body (truncation is
/// visible to the client); nothing is retried after commit.
fn relay_stream(
    response: reqwest::Response,
    idle: Duration,
    route: Route,
    key_id: Arc<str>,
) -> impl Stream<Item = Result<Bytes, RelayError>> + Send + 'static {
    futures_util::stream::unfold(Some(response), move |state| {
        let key_id = Arc::clone(&key_id);
        async move {
            let mut response = state?;
            match tokio::time::timeout(idle, response.chunk()).await {
                Ok(Ok(Some(chunk))) => Some((Ok(chunk), Some(response))),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    tracing::warn!(route = route.name(), key = %key_id, %error, "upstream body failed after commit; client stream aborted");
                    Some((Err(RelayError::Upstream(error)), None))
                }
                Err(_) => {
                    tracing::warn!(route = route.name(), key = %key_id, idle_secs = idle.as_secs(), "upstream stream idle; client stream aborted");
                    Some((Err(RelayError::Idle(idle)), None))
                }
            }
        }
    })
}

async fn read_body(
    body: Body,
    headers: &HeaderMap,
    limits: &RequestLimits,
) -> Result<Bytes, ProxyError> {
    let limit = limits.max_request_bytes;
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX)) {
        return Err(ProxyError::BodyTooLarge { limit });
    }
    let collect = async {
        let mut stream = body.into_data_stream();
        let mut buffer = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ProxyError::BodyRead)?;
            if buffer.len().saturating_add(chunk.len()) > limit {
                return Err(ProxyError::BodyTooLarge { limit });
            }
            buffer.extend_from_slice(&chunk);
        }
        Ok::<Bytes, ProxyError>(buffer.freeze())
    };
    tokio::time::timeout(limits.request_read_timeout, collect)
        .await
        .map_err(|_| ProxyError::BodyTimeout(limits.request_read_timeout))?
}

#[derive(Deserialize)]
struct ModelProbe<'a> {
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
}

fn request_model(payload: &[u8]) -> Result<String, ProxyError> {
    let probe: ModelProbe<'_> =
        serde_json::from_slice(payload).map_err(|_| ProxyError::InvalidJson)?;
    match probe.model {
        Some(model) if !model.trim().is_empty() && model.len() <= MAX_MODEL_LEN => {
            Ok(model.into_owned())
        }
        _ => Err(ProxyError::MissingModel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_probe_reads_only_the_model() {
        let body = br#"{"model":"kimi-k2","messages":[{"role":"user","content":"hi"}],"tools":[]}"#;
        assert_eq!(request_model(body).expect("model"), "kimi-k2");
        assert!(matches!(
            request_model(br#"{"messages":[]}"#),
            Err(ProxyError::MissingModel)
        ));
        assert!(matches!(
            request_model(br#"{"model":7}"#),
            Err(ProxyError::InvalidJson)
        ));
        assert!(matches!(
            request_model(b"[1]"),
            Err(ProxyError::InvalidJson)
        ));
    }

    #[test]
    fn credentials_and_identity_headers_are_not_forwarded_from_clients() {
        for name in [
            "authorization",
            "x-api-key",
            "cookie",
            "x-opencode-session",
            "session_id",
            "accept-encoding",
        ] {
            assert!(!forwardable(&HeaderName::from_static(name)), "{name}");
        }
        for name in ["content-type", "anthropic-version", "x-opencode-request"] {
            assert!(forwardable(&HeaderName::from_static(name)), "{name}");
        }
    }
}
