//! Commit boundary, cancellation ownership and shutdown.

use std::io;

use super::*;

const COMMITTED: [&str; 3] = [
    concat!(
        r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"step one"}}]}"#,
        "\n\n"
    ),
    concat!(
        r#"data: {"choices":[{"index":0,"delta":{"content":"ORQEN_"}}]}"#,
        "\n\n"
    ),
    concat!(
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"multiply","arguments":"{\"a\":19"}}]}}]}"#,
        "\n\n"
    ),
];

#[tokio::test]
async fn committed_output_is_never_followed_by_another_attempt() {
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A, KEY_B])).await;
    let (sender, streaming) = streamed(&[("content-type", "text/event-stream")]);
    upstream.push(Reply::Now(streaming));
    // Bait: a retry would get this from the other key.
    upstream.push(ok_json(r#"{"bait":"OTHER_KEY_OUTPUT"}"#));
    let pending = tokio::spawn(
        post(
            &client(),
            &orqen,
            "/v1/chat/completions",
            Some("conv-commit"),
            CHAT_BODY,
        )
        .send(),
    );
    let _first = upstream.next().await;

    let mut expected = Vec::new();
    let mut received = Vec::new();
    within(sender.send(Ok(Bytes::copy_from_slice(COMMITTED[0].as_bytes()))))
        .await
        .expect("upstream body open");
    expected.extend_from_slice(COMMITTED[0].as_bytes());
    let mut response = within(pending)
        .await
        .expect("client task")
        .expect("client response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    read_at_least(&mut response, &mut received, expected.len()).await;
    for fragment in &COMMITTED[1..] {
        within(sender.send(Ok(Bytes::copy_from_slice(fragment.as_bytes()))))
            .await
            .expect("upstream body open");
        expected.extend_from_slice(fragment.as_bytes());
        read_at_least(&mut response, &mut received, expected.len()).await;
    }
    within(sender.send(Err(io::Error::other("upstream reset after output"))))
        .await
        .expect("upstream body open");

    let outcome = read_rest(&mut response, &mut received).await;
    assert!(
        outcome.is_err(),
        "client must see the truncation as a body error"
    );
    assert_eq!(text(&received), text(&expected));
    assert!(!text(&received).contains("OTHER_KEY_OUTPUT"));
    upstream.assert_idle();
}

#[tokio::test]
async fn premature_upstream_eof_after_output_truncates_without_retry() {
    let partial = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"multiply","arguments":"{\"a\":19"#;
    let canned = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\n\r\n{partial}"
    );
    let mut raw = RawUpstream::start(canned.into_bytes(), AfterWrite::CloseWhenReleased).await;
    let orqen = start(&Setup::new(&raw.base_url, &[KEY_A, KEY_B])).await;
    let mut response = within(
        post(
            &client(),
            &orqen,
            "/v1/chat/completions",
            Some("conv-eof"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response head");
    assert!(matches!(raw.next().await, RawEvent::Request));
    assert_eq!(response.status(), StatusCode::OK);
    let mut received = Vec::new();
    // The client must hold the partial output before upstream closes early.
    read_at_least(&mut response, &mut received, partial.len()).await;
    assert_eq!(text(&received), partial);
    raw.release_close();
    let outcome = read_rest(&mut response, &mut received).await;
    assert!(
        outcome.is_err(),
        "premature EOF must surface as a body error"
    );
    assert_eq!(text(&received), partial);
    raw.assert_idle();
}

#[tokio::test]
async fn client_disconnect_after_output_drops_the_upstream_body() {
    let data = concat!(
        r#"data: {"choices":[{"delta":{"content":"partial-output"}}]}"#,
        "\n\n"
    );
    let canned = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{data}\r\n",
        data.len()
    );
    let mut raw = RawUpstream::start(canned.into_bytes(), AfterWrite::AwaitPeerClose).await;
    let orqen = start(&Setup::new(&raw.base_url, &[KEY_A, KEY_B])).await;
    let mut stream = raw_post(
        orqen.addr,
        "/v1/chat/completions",
        "x-opencode-session: conv-drop\r\n",
        CHAT_BODY,
    )
    .await;
    assert!(matches!(raw.next().await, RawEvent::Request));
    read_until_contains(&mut stream, b"partial-output").await;
    drop(stream);
    assert!(
        matches!(raw.next().await, RawEvent::Closed),
        "the upstream body must be dropped with the client"
    );
    orqen.stop().await;
    raw.assert_idle();
}

#[tokio::test]
async fn client_disconnect_before_any_response_starts_no_new_attempt() {
    let mut raw = RawUpstream::start(Vec::new(), AfterWrite::AwaitPeerClose).await;
    let orqen = start(&Setup::new(&raw.base_url, &[KEY_A, KEY_B])).await;
    let stream = raw_post(
        orqen.addr,
        "/v1/chat/completions",
        "x-opencode-session: conv-cancel\r\n",
        CHAT_BODY,
    )
    .await;
    assert!(matches!(raw.next().await, RawEvent::Request));
    drop(stream);
    assert!(
        matches!(raw.next().await, RawEvent::Closed),
        "the in-flight upstream request must be dropped with the client"
    );
    // Graceful stop completes only when no handler future (and therefore
    // no attempt loop) remains; only then is "no second attempt" final.
    orqen.stop().await;
    raw.assert_idle();
}

#[tokio::test]
async fn shutdown_prevents_any_further_attempt() {
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A, KEY_B])).await;
    let client = client();
    let (release, later) = oneshot::channel::<Response>();
    upstream.push(Reply::Later(later));
    // Bait for a retry that must not happen.
    upstream.push(ok_json("{}"));
    let pending = tokio::spawn(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-shutdown"),
            CHAT_BODY,
        )
        .send(),
    );
    let _first = upstream.next().await;
    orqen.state.shutdown.cancel();
    assert!(
        release
            .send(respond(
                StatusCode::TOO_MANY_REQUESTS,
                &[("retry-after", "5")],
                r#"{"error":"rate"}"#,
            ))
            .is_ok(),
        "upstream handler is waiting for its reply"
    );
    let response = within(pending)
        .await
        .expect("client task")
        .expect("client response");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    upstream.assert_idle();

    let refused = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-late"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&refused, "x-orqen-error"), Some("1"));
    upstream.assert_idle();

    let (status, health) = health(&client, &orqen).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["status"], "draining");
}
