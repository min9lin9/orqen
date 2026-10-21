//! Missing/rejected/ephemeral identity contracts and request limits.

use super::*;

#[tokio::test]
async fn missing_identity_is_rejected_under_the_reject_policy() {
    let mut upstream = Upstream::start().await;
    let orqen =
        start(&Setup::new(&upstream.base_url, &[KEY_A]).session("missing = \"reject\"")).await;
    let client = client();
    let response = within(post(&client, &orqen, "/v1/chat/completions", None, CHAT_BODY).send())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, "x-orqen-error"), Some("1"));
    assert_eq!(
        json_body(response).await["error"]["type"],
        "invalid_request_error"
    );
    upstream.assert_idle();

    upstream.push(ok_json("{}"));
    let accepted = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-explicit"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(
        upstream.next().await.header("x-opencode-session"),
        Some("conv-explicit")
    );
}

#[tokio::test]
async fn missing_identity_gets_a_fresh_ephemeral_session_per_request() {
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A])).await;
    let client = client();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        upstream.push(ok_json("{}"));
        let response =
            within(post(&client, &orqen, "/v1/chat/completions", None, CHAT_BODY).send())
                .await
                .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let captured = upstream.next().await;
        let session = captured
            .header("x-opencode-session")
            .expect("ephemeral session forwarded")
            .to_owned();
        assert!(session.starts_with("orqen-ephemeral-"), "{session}");
        sessions.push(session);
    }
    assert_ne!(
        sessions[0], sessions[1],
        "unrelated requests must not share a session"
    );
    let (_, health) = health(&client, &orqen).await;
    assert_eq!(health["ephemeral_sessions"], 2);
}

#[tokio::test]
async fn ambiguous_identity_is_rejected() {
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A])).await;
    let mut headers = HeaderMap::new();
    headers.append("x-opencode-session", HeaderValue::from_static("conv-1"));
    headers.append("x-opencode-session", HeaderValue::from_static("conv-2"));
    let response = within(
        client()
            .post(orqen.url("/v1/chat/completions"))
            .headers(headers)
            .header("content-type", "application/json")
            .body(CHAT_BODY)
            .send(),
    )
    .await
    .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, "x-orqen-error"), Some("1"));
    upstream.assert_idle();
}

#[tokio::test]
async fn request_limits_and_error_shapes_apply_before_upstream() {
    let mut upstream = Upstream::start().await;
    let orqen =
        start(&Setup::new(&upstream.base_url, &[KEY_A]).limits("max_request_bytes = 1024")).await;
    let client = client();

    let oversized = format!(r#"{{"model":"m","pad":"{}"}}"#, "x".repeat(2048));
    let response = within(
        client
            .post(orqen.url("/v1/chat/completions"))
            .header("content-type", "application/json")
            .header("x-opencode-session", "conv-limits")
            .body(oversized)
            .send(),
    )
    .await
    .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header(&response, "x-orqen-error"), Some("1"));

    let missing_model = within(
        post(
            &client,
            &orqen,
            "/v1/messages",
            Some("conv-limits"),
            r#"{"messages":[]}"#,
        )
        .send(),
    )
    .await
    .expect("response");
    assert_eq!(missing_model.status(), StatusCode::BAD_REQUEST);
    let body = json_body(missing_model).await;
    assert_eq!(
        body["type"], "error",
        "Anthropic error shape on /v1/messages"
    );
    assert_eq!(body["error"]["type"], "invalid_request_error");

    let not_json = within(
        post(
            &client,
            &orqen,
            "/v1/responses",
            Some("conv-limits"),
            "not json",
        )
        .send(),
    )
    .await
    .expect("response");
    assert_eq!(not_json.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(not_json).await["error"]["type"],
        "invalid_request_error"
    );
    upstream.assert_idle();
}

#[tokio::test]
async fn health_reports_public_ids_without_secrets() {
    let upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &[KEY_A, SHARED_1])).await;
    let response = within(client().get(orqen.url("/healthz")).send())
        .await
        .expect("health");
    assert_eq!(response.status(), StatusCode::OK);
    let raw = body_text(response).await;
    assert!(!raw.contains("sk-"), "health must not expose secrets");
    let body: Value = serde_json::from_str(&raw).expect("health json");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["keys"][0]["id"], "go-a");
    assert_eq!(body["keys"][0]["scope"], "acct-a");
    assert_eq!(body["keys"][1]["id"], "go-shared-1");
    assert_eq!(body["keys"][1]["scope"], "acct-shared");
}
