//! Native path mapping, byte-exact request/response relay, credential
//! replacement and opaque Content-Encoding.

use std::io::{Read as _, Write as _};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use super::*;

const MESSAGES_TOOLS: &str = r#"{"model":"deepseek-v4.1-flash","max_tokens":1024,"stream":true,"thinking":{"type":"enabled","budget_tokens":512},"messages":[{"role":"user","content":"multiply 19 by 23"},{"role":"assistant","content":[{"type":"thinking","thinking":"opaque","signature":"c2lnbmF0dXJlLW9wYXF1ZQ=="},{"type":"redacted_thinking","data":"ZW5jcnlwdGVk"},{"type":"tool_use","id":"toolu_19x23","name":"multiply","input":{"a":19,"b":{"nested":[23]}}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_19x23","content":"437"}]}],"tools":[{"name":"multiply","input_schema":{"type":"object"}}],"metadata":{"user_id":"opaque"},"x_extra":true}"#;

const RESPONSES_TOOLS: &str = r#"{"model":"deepseek-v4.1-flash","stream":true,"input":[{"role":"user","content":"multiply"},{"type":"reasoning","id":"rs_1","encrypted_content":"gAAAAopaque==","summary":[]},{"type":"function_call","call_id":"call_19x23","name":"multiply","arguments":"{\"a\":19,\"b\":23}"},{"type":"function_call_output","call_id":"call_19x23","output":"437"}],"tools":[{"type":"function","name":"multiply","parameters":{"type":"object"}}],"include":["reasoning.encrypted_content"],"store":false,"x_extra":[null]}"#;

// Fragments deliberately split inside JSON strings and events.
const CHAT_SSE: [&str; 4] = [
    r#"data: {"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"thi"#,
    concat!(
        r#"nk"}}]}"#,
        "\n\n",
        r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"multiply","arguments":"{\"a\":"#
    ),
    concat!(
        r#"19}"}}]}}]}"#,
        "\n\n",
        r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"437"},"finish_reason":"tool_calls"}]}"#,
        "\n\n"
    ),
    "data: [DONE]\n\n",
];

const MESSAGES_SSE: [&str; 4] = [
    concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[]}}"#,
        "\n\nevent: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        "\n\n"
    ),
    concat!(
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"c2ln"#
    ),
    concat!(
        r#"bmF0dXJl"}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":19,"}}"#,
        "\n\n"
    ),
    concat!(
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n"
    ),
];

const RESPONSES_SSE: [&str; 3] = [
    concat!(
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_2","encrypted_content":"gAAA"#
    ),
    concat!(
        r#"Bopaque=="}}"#,
        "\n\nevent: response.function_call_arguments.delta\n",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"a\":19"}"#,
        "\n\n"
    ),
    concat!(
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#,
        "\n\n"
    ),
];

#[derive(Clone, Copy)]
enum Auth {
    Bearer,
    ApiKey,
}

async fn round_trip(
    route: &str,
    upstream_path: &str,
    body: &'static str,
    fragments: &[&str],
    auth: Auth,
) {
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A])).await;
    let (sender, response) = streamed(&[
        ("content-type", "text/event-stream"),
        ("x-upstream-marker", "kept"),
    ]);
    upstream.push(Reply::Now(response));

    let request = client()
        .post(orqen.url(route))
        .header("content-type", "application/json")
        .header("authorization", "Bearer client-local-sentinel")
        .header("x-api-key", "client-local-sentinel")
        .header("anthropic-version", "2023-06-01")
        .header("x-opencode-session", "ses-route")
        .body(body);
    let pending = tokio::spawn(request.send());

    let captured = upstream.next().await;
    assert_eq!(captured.method, Method::POST);
    assert_eq!(captured.path, upstream_path);
    assert_eq!(
        text(&captured.body),
        body,
        "request bytes must be unchanged"
    );
    assert_eq!(captured.header("content-type"), Some("application/json"));
    assert_eq!(captured.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(captured.header("x-opencode-session"), Some("ses-route"));
    match auth {
        Auth::Bearer => {
            assert_eq!(captured.header("authorization"), Some("Bearer sk-test-a"));
            assert_eq!(captured.header("x-api-key"), None);
        }
        Auth::ApiKey => {
            assert_eq!(captured.header("x-api-key"), Some("sk-test-a"));
            assert_eq!(captured.header("authorization"), None);
        }
    }

    let mut expected = Vec::new();
    let mut received = Vec::new();
    let (first, rest) = fragments.split_first().expect("at least one fragment");
    within(sender.send(Ok(Bytes::copy_from_slice(first.as_bytes()))))
        .await
        .expect("upstream body open");
    expected.extend_from_slice(first.as_bytes());
    let mut response = within(pending)
        .await
        .expect("client task")
        .expect("client response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), Some("text/event-stream"));
    assert_eq!(header(&response, "x-upstream-marker"), Some("kept"));
    assert_eq!(header(&response, "x-orqen-key"), Some("go-a"));
    assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    read_at_least(&mut response, &mut received, expected.len()).await;
    assert_eq!(text(&received), text(&expected));
    for fragment in rest {
        within(sender.send(Ok(Bytes::copy_from_slice(fragment.as_bytes()))))
            .await
            .expect("upstream body open");
        expected.extend_from_slice(fragment.as_bytes());
        // The next fragment is produced only after the client holds this
        // one: the relay streams instead of buffering.
        read_at_least(&mut response, &mut received, expected.len()).await;
        assert_eq!(text(&received), text(&expected));
    }
    drop(sender);
    read_rest(&mut response, &mut received)
        .await
        .expect("clean end of stream");
    assert_eq!(text(&received), fragments.concat());
    upstream.assert_idle();
}

#[tokio::test]
async fn chat_completions_are_relayed_natively() {
    round_trip(
        "/v1/chat/completions",
        "/openai/v1/chat/completions",
        CHAT_TOOLS,
        &CHAT_SSE,
        Auth::Bearer,
    )
    .await;
}

#[tokio::test]
async fn messages_are_relayed_natively_with_x_api_key() {
    round_trip(
        "/v1/messages",
        "/anthropic/v1/messages",
        MESSAGES_TOOLS,
        &MESSAGES_SSE,
        Auth::ApiKey,
    )
    .await;
}

#[tokio::test]
async fn responses_are_relayed_natively() {
    round_trip(
        "/v1/responses",
        "/openai/v1/responses",
        RESPONSES_TOOLS,
        &RESPONSES_SSE,
        Auth::Bearer,
    )
    .await;
}

#[tokio::test]
async fn models_are_listed_through_the_native_path() {
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A])).await;
    let listing = r#"{"object":"list","data":[{"id":"deepseek-v4.1-flash","object":"model"}]}"#;
    upstream.push(ok_json(listing));
    let response = within(
        client()
            .get(orqen.url("/v1/models"))
            .header("authorization", "Bearer client-local-sentinel")
            .send(),
    )
    .await
    .expect("models response");
    let captured = upstream.next().await;
    assert_eq!(captured.method, Method::GET);
    assert_eq!(captured.path, "/v1/models");
    assert_eq!(captured.header("authorization"), Some("Bearer sk-test-a"));
    assert_eq!(captured.header("x-opencode-session"), None);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, listing);
    upstream.assert_idle();
}

#[tokio::test]
async fn compressed_responses_keep_encoding_and_exact_bytes() {
    let original = r#"{"id":"c1","choices":[{"message":{"content":"ORQEN_GZIP"}}]}"#;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(original.as_bytes()).expect("gzip write");
    let compressed = encoder.finish().expect("gzip finish");

    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A])).await;
    upstream.push(Reply::Now(respond(
        StatusCode::OK,
        &[
            ("content-type", "application/json"),
            ("content-encoding", "gzip"),
        ],
        compressed.clone(),
    )));
    let response = within(
        post(
            &client(),
            &orqen,
            "/v1/chat/completions",
            Some("ses-gzip"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let captured = upstream.next().await;
    assert_eq!(
        captured.header("accept-encoding"),
        None,
        "Orqen must not negotiate its own decompression"
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-encoding"), Some("gzip"));
    let body = within(response.bytes()).await.expect("body");
    assert_eq!(
        body.to_vec(),
        compressed,
        "encoded bytes must be relayed unchanged"
    );
    let mut decoded = String::new();
    GzDecoder::new(body.as_ref())
        .read_to_string(&mut decoded)
        .expect("client can decode the relayed gzip");
    assert_eq!(decoded, original);
}
