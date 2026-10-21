//! Loopback behavioural tests.
//!
//! Every test runs real HTTP servers on 127.0.0.1: a test client, Orqen's
//! actual config loader, router and `axum::serve` stack, and a scripted fake
//! upstream (axum, or raw TCP where exact connection-close observation is
//! needed). All upstreams here are mocks; nothing in this module is an
//! account end-to-end run.
//!
//! Synchronisation is event-driven: channels and oneshots are created before
//! the triggering action and every wait is bounded by [`WAIT`]. There are no
//! sleeps. Negative assertions ("no further upstream request") are made only
//! after an exact completion event — the client receiving its final
//! response or body error, or Orqen's graceful serve task finishing, which
//! happens only once every handler future and attempt loop is gone.

mod commit;
mod forwarding;
mod pool_retry;
mod session_limits;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::proxy::AppState;

/// Upper bound for every awaited event. A hang fails the test; it is never
/// used as a delay.
const WAIT: Duration = Duration::from_secs(10);

async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("expected event did not happen within the test bound")
}

#[derive(Clone, Copy)]
struct TestKey {
    id: &'static str,
    scope: &'static str,
    secret: &'static str,
}

const KEY_A: TestKey = TestKey {
    id: "go-a",
    scope: "acct-a",
    secret: "sk-test-a",
};
const KEY_B: TestKey = TestKey {
    id: "go-b",
    scope: "acct-b",
    secret: "sk-test-b",
};
const KEY_C: TestKey = TestKey {
    id: "go-c",
    scope: "acct-c",
    secret: "sk-test-c",
};
const SHARED_1: TestKey = TestKey {
    id: "go-shared-1",
    scope: "acct-shared",
    secret: "sk-shared-1",
};
const SHARED_2: TestKey = TestKey {
    id: "go-shared-2",
    scope: "acct-shared",
    secret: "sk-shared-2",
};

const CHAT_BODY: &str =
    r#"{"model":"deepseek-v4.1-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
const OTHER_MODEL_BODY: &str =
    r#"{"model":"other-model","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
/// Tool call, tool result, nested arguments, opaque reasoning, vendor extras
/// and non-canonical spacing: any re-serialisation would change these bytes.
const CHAT_TOOLS: &str = r#"{"model":"deepseek-v4.1-flash","stream":true,"messages":[{"role":"user","content":"multiply 19 by 23"},{"role":"assistant","content":null,"reasoning_content":"opaque\u00e9-reasoning","tool_calls":[{"id":"call_19x23","type":"function","function":{"name":"multiply","arguments":"{\"a\":19,\"b\":{\"nested\":[23,{\"deep\":true}]}}"}}]},{"role":"tool","tool_call_id":"call_19x23","content":"437"}],"tools":[{"type":"function","function":{"name":"multiply","parameters":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"object"}}}}}],"tool_choice":"auto","x_vendor_extra":{"keep":[1,2.50,"x"]} ,  "spacing_kept" : 1.0}"#;

/// Public key id of the credential an upstream request carried.
fn key_of(captured: &Captured, keys: &[TestKey]) -> &'static str {
    let bearer = captured
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key = captured.header("x-api-key");
    keys.iter()
        .find(|key| bearer == Some(key.secret) || api_key == Some(key.secret))
        .map(|key| key.id)
        .expect("upstream request carried a configured credential")
}

struct TempDir(PathBuf);

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "orqen-it-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("keys")).expect("create test dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Real TOML config plus key files written to a private temp dir.
struct Setup {
    base_url: String,
    keys: Vec<TestKey>,
    upstream: String,
    limits: String,
    session: String,
}

impl Setup {
    fn new(base_url: &str, keys: &[TestKey]) -> Self {
        Self {
            base_url: base_url.to_owned(),
            keys: keys.to_vec(),
            upstream: String::new(),
            limits: String::new(),
            session: String::new(),
        }
    }

    fn upstream(mut self, line: &str) -> Self {
        self.upstream.push_str(line);
        self.upstream.push('\n');
        self
    }

    fn limits(mut self, line: &str) -> Self {
        self.limits.push_str(line);
        self.limits.push('\n');
        self
    }

    fn session(mut self, line: &str) -> Self {
        self.session.push_str(line);
        self.session.push('\n');
        self
    }

    fn write(&self) -> TempDir {
        let dir = TempDir::new();
        let mut text = format!(
            "listen = \"127.0.0.1:0\"\n\n[upstream]\nbase_url = \"{}\"\n{}\n[limits]\n{}\n[session]\n{}\n",
            self.base_url, self.upstream, self.limits, self.session
        );
        for key in &self.keys {
            std::fs::write(
                dir.0.join("keys").join(format!("{}.key", key.id)),
                format!("{}\n", key.secret),
            )
            .expect("write key file");
            text.push_str(&format!(
                "[[keys]]\nid = \"{}\"\nkey_file = \"keys/{}.key\"\nscope = \"{}\"\n\n",
                key.id, key.id, key.scope
            ));
        }
        std::fs::write(dir.0.join("orqen.toml"), text).expect("write config");
        dir
    }
}

fn load(setup: &Setup) -> (TempDir, Config) {
    let dir = setup.write();
    let config = crate::config::load(&dir.0.join("orqen.toml")).expect("test config loads");
    (dir, config)
}

/// A running Orqen instance on an ephemeral loopback port.
struct Orqen {
    addr: SocketAddr,
    state: Arc<AppState>,
    stop: CancellationToken,
    served: JoinHandle<std::io::Result<()>>,
    _dir: TempDir,
}

impl Orqen {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Graceful stop; resolves only after every connection, and therefore
    /// every handler future and attempt loop, has finished.
    async fn stop(self) {
        let Self { stop, served, .. } = self;
        stop.cancel();
        within(served)
            .await
            .expect("serve task joins")
            .expect("serve ends cleanly");
    }
}

async fn serve(router: Router, state: Arc<AppState>, dir: TempDir) -> Orqen {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind orqen");
    let addr = listener.local_addr().expect("orqen address");
    let stop = CancellationToken::new();
    let served = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(stop.clone().cancelled_owned())
            .into_future(),
    );
    Orqen {
        addr,
        state,
        stop,
        served,
        _dir: dir,
    }
}

/// Same assembly path as `main`: config file → `build_service` → serve.
async fn start(setup: &Setup) -> Orqen {
    let (dir, config) = load(setup);
    let service = crate::build_service(config, CancellationToken::new()).expect("service builds");
    serve(service.router, service.state, dir).await
}

/// Test client: no proxy, no transparent decompression.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()
        .expect("test client")
}

fn post(
    client: &reqwest::Client,
    orqen: &Orqen,
    path: &str,
    session: Option<&str>,
    body: &'static str,
) -> reqwest::RequestBuilder {
    let builder = client
        .post(orqen.url(path))
        .header("content-type", "application/json")
        .body(body);
    match session {
        Some(session) => builder.header("x-opencode-session", session),
        None => builder,
    }
}

fn header<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn retry_after_secs(response: &reqwest::Response) -> u64 {
    header(response, "retry-after")
        .and_then(|value| value.parse().ok())
        .expect("numeric Retry-After")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf-8 bytes")
}

async fn body_text(response: reqwest::Response) -> String {
    let bytes = within(response.bytes()).await.expect("response body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

async fn json_body(response: reqwest::Response) -> Value {
    let bytes = within(response.bytes()).await.expect("response body");
    serde_json::from_slice(&bytes).expect("json body")
}

async fn health(client: &reqwest::Client, orqen: &Orqen) -> (StatusCode, Value) {
    let response = within(client.get(orqen.url("/healthz")).send())
        .await
        .expect("health response");
    let status = response.status();
    (status, json_body(response).await)
}

async fn read_at_least(response: &mut reqwest::Response, buffer: &mut Vec<u8>, length: usize) {
    while buffer.len() < length {
        let chunk = within(response.chunk())
            .await
            .expect("client body chunk")
            .expect("client body ended early");
        buffer.extend_from_slice(&chunk);
    }
}

async fn read_rest(
    response: &mut reqwest::Response,
    buffer: &mut Vec<u8>,
) -> Result<(), reqwest::Error> {
    while let Some(chunk) = within(response.chunk()).await? {
        buffer.extend_from_slice(&chunk);
    }
    Ok(())
}

/// One request as the fake upstream saw it.
struct Captured {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

enum Reply {
    Now(Response),
    /// Respond with whatever the test sends after observing the request.
    Later(oneshot::Receiver<Response>),
}

type Script = Arc<Mutex<VecDeque<Reply>>>;

/// Scripted axum upstream: replies are consumed in request order and every
/// request is reported on `captured` before its reply is produced.
struct Upstream {
    base_url: String,
    captured: mpsc::UnboundedReceiver<Captured>,
    script: Script,
}

impl Upstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake upstream");
        let addr = listener.local_addr().expect("fake upstream address");
        let (sender, captured) = mpsc::unbounded_channel();
        let script: Script = Arc::default();
        let handler_script = Arc::clone(&script);
        let app = Router::new().fallback(move |request: Request| {
            scripted(request, sender.clone(), Arc::clone(&handler_script))
        });
        tokio::spawn(axum::serve(listener, app).into_future());
        Self {
            base_url: format!("http://{addr}"),
            captured,
            script,
        }
    }

    fn push(&self, reply: Reply) {
        self.script.lock().expect("script lock").push_back(reply);
    }

    async fn next(&mut self) -> Captured {
        within(self.captured.recv())
            .await
            .expect("fake upstream stopped")
    }

    fn assert_idle(&mut self) {
        assert!(
            matches!(self.captured.try_recv(), Err(TryRecvError::Empty)),
            "unexpected upstream request"
        );
    }
}

async fn scripted(
    request: Request,
    captured: mpsc::UnboundedSender<Captured>,
    script: Script,
) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX)
        .await
        .expect("fake upstream reads body");
    let reply = script.lock().expect("script lock").pop_front();
    let _ = captured.send(Captured {
        method: parts.method,
        path: parts.uri.path().to_owned(),
        headers: parts.headers,
        body,
    });
    match reply {
        Some(Reply::Now(response)) => response,
        Some(Reply::Later(pending)) => pending.await.unwrap_or_else(|_| {
            respond(
                StatusCode::INTERNAL_SERVER_ERROR,
                &[],
                "reply sender dropped",
            )
        }),
        None => respond(StatusCode::IM_A_TEAPOT, &[], "unscripted upstream request"),
    }
}

fn respond(
    status: StatusCode,
    headers: &[(&'static str, &str)],
    body: impl Into<Body>,
) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    for &(name, value) in headers {
        response.headers_mut().append(
            name,
            HeaderValue::from_str(value).expect("valid test header"),
        );
    }
    response
}

fn reply(status: StatusCode, headers: &[(&'static str, &str)], body: &'static str) -> Reply {
    Reply::Now(respond(status, headers, body))
}

fn ok_json(body: &'static str) -> Reply {
    reply(
        StatusCode::OK,
        &[("content-type", "application/json")],
        body,
    )
}

type Chunk = Result<Bytes, std::io::Error>;

/// 200 response whose body chunks are produced by the test, one at a time
/// (channel capacity 1), so the test controls fragmentation and failure.
fn streamed(headers: &[(&'static str, &str)]) -> (mpsc::Sender<Chunk>, Response) {
    let (sender, receiver) = mpsc::channel::<Chunk>(1);
    let body = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    (
        sender,
        respond(StatusCode::OK, headers, Body::from_stream(body)),
    )
}

enum RawEvent {
    /// A complete request was read.
    Request,
    /// The peer (Orqen's upstream connection) closed the socket.
    Closed,
}

#[derive(Clone, Copy)]
enum AfterWrite {
    /// Close (FIN) only after the test calls [`RawUpstream::release_close`],
    /// i.e. after it has observed what the client already received.
    CloseWhenReleased,
    /// Keep the connection open and report when Orqen closes it.
    AwaitPeerClose,
}

/// Raw TCP upstream writing canned bytes; observes connection close exactly.
struct RawUpstream {
    base_url: String,
    events: mpsc::UnboundedReceiver<RawEvent>,
    release: Arc<Notify>,
}

impl RawUpstream {
    async fn start(response: Vec<u8>, after: AfterWrite) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw upstream");
        let addr = listener.local_addr().expect("raw upstream address");
        let (sender, events) = mpsc::unbounded_channel();
        let response = Arc::new(response);
        let release = Arc::new(Notify::new());
        let connection_release = Arc::clone(&release);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(raw_connection(
                    socket,
                    sender.clone(),
                    Arc::clone(&response),
                    after,
                    Arc::clone(&connection_release),
                ));
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            events,
            release,
        }
    }

    /// Let a `CloseWhenReleased` connection close. `notify_one` stores a
    /// permit, so the release is never lost regardless of ordering.
    fn release_close(&self) {
        self.release.notify_one();
    }

    async fn next(&mut self) -> RawEvent {
        within(self.events.recv())
            .await
            .expect("raw upstream stopped")
    }

    fn assert_idle(&mut self) {
        assert!(
            matches!(self.events.try_recv(), Err(TryRecvError::Empty)),
            "unexpected raw upstream activity"
        );
    }
}

async fn raw_connection(
    mut socket: TcpStream,
    events: mpsc::UnboundedSender<RawEvent>,
    response: Arc<Vec<u8>>,
    after: AfterWrite,
    release: Arc<Notify>,
) {
    read_http_request(&mut socket).await;
    let _ = events.send(RawEvent::Request);
    if socket.write_all(&response).await.is_err() {
        let _ = events.send(RawEvent::Closed);
        return;
    }
    match after {
        AfterWrite::CloseWhenReleased => {
            release.notified().await;
            let _ = socket.shutdown().await;
        }
        AfterWrite::AwaitPeerClose => {
            let mut scratch = [0_u8; 1024];
            while matches!(socket.read(&mut scratch).await, Ok(read) if read > 0) {}
            let _ = events.send(RawEvent::Closed);
        }
    }
}

/// Consume one request (head plus `content-length` body) so closing the
/// socket afterwards never resets unread data.
async fn read_http_request(socket: &mut TcpStream) {
    let mut data = Vec::new();
    let mut scratch = [0_u8; 4096];
    loop {
        if let Some(end) = find(&data, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                return;
            }
        }
        match socket.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(read) => data.extend_from_slice(&scratch[..read]),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Raw HTTP/1.1 client: dropping the returned socket is an exact client
/// disconnect.
async fn raw_post(addr: SocketAddr, path: &str, headers: &str, body: &str) -> TcpStream {
    let mut stream = within(TcpStream::connect(addr))
        .await
        .expect("connect to orqen");
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{headers}\r\n{body}",
        body.len()
    );
    within(stream.write_all(request.as_bytes()))
        .await
        .expect("write request");
    stream
}

async fn read_until_contains(stream: &mut TcpStream, needle: &[u8]) {
    let mut data = Vec::new();
    let mut scratch = [0_u8; 4096];
    while find(&data, needle).is_none() {
        let read = within(stream.read(&mut scratch))
            .await
            .expect("read from orqen");
        assert!(read > 0, "orqen closed before the expected output");
        data.extend_from_slice(&scratch[..read]);
    }
}
