//! Affinity, session propagation, scoped cooldowns and pre-commit retry.
//!
//! Cooldown timing is checked against exact brackets: the instants taken
//! just before sending and just after receiving a response bound when the
//! pool recorded or read a deadline, and the pool is queried at injected
//! instants. A slow host widens the bracket by exactly the elapsed time; it
//! never turns valid behaviour into a failure.

use std::time::{Instant, SystemTime};

use crate::pool::ceil_secs;

use super::*;

/// Range of a `Retry-After` computed from `deadline` at some instant within
/// `[earliest, latest]` (rounded up, never below one second).
fn retry_after_bounds(deadline: Instant, earliest: Instant, latest: Instant) -> (u64, u64) {
    let secs = |at: Instant| ceil_secs(deadline.saturating_duration_since(at)).max(1);
    (secs(latest), secs(earliest))
}

fn assert_within((low, high): (u64, u64), value: u64) {
    assert!(low <= value && value <= high, "{low} <= {value} <= {high}");
}

#[tokio::test]
async fn affinity_is_stable_and_conversation_is_forwarded() {
    let keys = [KEY_A, KEY_B];
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &keys)).await;
    let client = client();
    let mut chosen: Option<&'static str> = None;
    for round in 0..3 {
        upstream.push(ok_json("{}"));
        // omo's `sessionAffinityFormat = "openai"` header set.
        let response = within(
            client
                .post(orqen.url("/v1/chat/completions"))
                .header("content-type", "application/json")
                .header("session_id", "omo-conv-7")
                .header("x-client-request-id", format!("req-{round}"))
                .header("x-session-affinity", "omo-conv-7")
                .body(CHAT_BODY)
                .send(),
        )
        .await
        .expect("response");
        let captured = upstream.next().await;
        let key = key_of(&captured, &keys);
        assert_eq!(*chosen.get_or_insert(key), key, "one conversation, one key");
        assert_eq!(captured.header("x-opencode-session"), Some("omo-conv-7"));
        assert_eq!(captured.header("session_id"), None);
        assert_eq!(captured.header("x-client-request-id"), None);
        assert_eq!(captured.header("x-session-affinity"), None);
        assert_eq!(header(&response, "x-orqen-key"), Some(key));
        assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    }
    upstream.assert_idle();
}

#[tokio::test]
async fn rate_limit_fails_over_to_an_independent_scope_with_identical_bytes() {
    let keys = [KEY_A, KEY_B];
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &keys)).await;
    let client = client();
    upstream.push(reply(
        StatusCode::TOO_MANY_REQUESTS,
        &[("retry-after", "30")],
        r#"{"error":"rate"}"#,
    ));
    upstream.push(ok_json(r#"{"answer":"second"}"#));
    let before = Instant::now();
    let response = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-failover"),
            CHAT_TOOLS,
        )
        .send(),
    )
    .await
    .expect("response");
    let after = Instant::now();
    let first = upstream.next().await;
    let second = upstream.next().await;
    assert_eq!(first.path, "/openai/v1/chat/completions");
    assert_eq!(second.path, first.path);
    assert_eq!(text(&first.body), CHAT_TOOLS);
    assert_eq!(second.body, first.body, "same model, same bytes");
    let (first_key, second_key) = (key_of(&first, &keys), key_of(&second, &keys));
    assert_ne!(first_key, second_key);
    for captured in [&first, &second] {
        assert_eq!(captured.header("x-opencode-session"), Some("conv-failover"));
    }
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-orqen-key"), Some(second_key));
    assert_eq!(header(&response, "x-orqen-attempts"), Some("2"));
    assert_eq!(body_text(response).await, r#"{"answer":"second"}"#);
    upstream.assert_idle();

    // The cooled scope stays out of rotation for this conversation.
    upstream.push(ok_json("{}"));
    let again = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-failover"),
            CHAT_TOOLS,
        )
        .send(),
    )
    .await
    .expect("response");
    let third = upstream.next().await;
    assert_eq!(key_of(&third, &keys), second_key);
    assert_eq!(header(&again, "x-orqen-attempts"), Some("1"));
    upstream.assert_idle();

    // The 429 was recorded at some instant in [before, after], so the scope
    // deadline lies in [before + 30s, after + 30s].
    let cooldown = Duration::from_secs(30);
    let query = Instant::now();
    let snapshot = orqen.state.pool.snapshot(query);
    let cooled = snapshot
        .iter()
        .find(|key| key.id == first_key)
        .expect("first key listed");
    let secs = cooled.scope_blocked_secs.expect("scope cooldown");
    let low = ceil_secs((before + cooldown).saturating_duration_since(query));
    let high = ceil_secs((after + cooldown).saturating_duration_since(query));
    assert!(low <= secs && secs <= high, "{low} <= {secs} <= {high}");

    // /healthz exposes the same deadline, read somewhere in [ask, answered].
    let ask = Instant::now();
    let (_, health) = health(&client, &orqen).await;
    let answered = Instant::now();
    let listed = health["keys"]
        .as_array()
        .expect("keys array")
        .iter()
        .find(|key| key["id"] == first_key)
        .expect("first key listed");
    let secs = listed["scope_blocked_secs"]
        .as_u64()
        .expect("scope cooldown");
    let low = ceil_secs((before + cooldown).saturating_duration_since(answered));
    let high = ceil_secs((after + cooldown).saturating_duration_since(ask));
    assert!(low <= secs && secs <= high, "{low} <= {secs} <= {high}");
}

#[tokio::test]
async fn keys_on_one_account_share_the_cooldown() {
    let keys = [SHARED_1, SHARED_2];
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &keys)).await;
    let client = client();
    let reset = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
    let reset_at = httpdate::parse_http_date(&reset).expect("valid HTTP date");
    upstream.push(reply(
        StatusCode::TOO_MANY_REQUESTS,
        &[("retry-after", &reset)],
        r#"{"error":{"type":"rate_limit"}}"#,
    ));
    let (wall_before, before) = (SystemTime::now(), Instant::now());
    let response = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-shared"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let (after, wall_after) = (Instant::now(), SystemTime::now());
    let only = upstream.next().await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    assert_eq!(header(&response, "x-orqen-key"), Some(key_of(&only, &keys)));
    assert_eq!(
        header(&response, "x-orqen-error"),
        None,
        "upstream rejection is relayed"
    );

    // Every key of the account is blocked, so the pool reports exactly
    // `deadline - query` for an injected query instant.
    let query = Instant::now();
    let remaining = orqen
        .state
        .pool
        .unavailable_for("deepseek-v4.1-flash", query)
        .expect("whole account cooled");
    let deadline = query + remaining;
    // deadline = report instant (in [before, after]) + (HTTP date - parse
    // wall time, parsed in [wall_before, wall_after]).
    assert!(
        deadline.saturating_duration_since(before)
            >= reset_at
                .duration_since(wall_after)
                .expect("date is in the future")
    );
    assert!(
        deadline.saturating_duration_since(after)
            <= reset_at
                .duration_since(wall_before)
                .expect("date is in the future")
    );
    // The relayed Retry-After was derived from that deadline in [before, after].
    assert_within(
        retry_after_bounds(deadline, before, after),
        retry_after_secs(&response),
    );
    assert_eq!(
        body_text(response).await,
        r#"{"error":{"type":"rate_limit"}}"#
    );
    upstream.assert_idle();

    // Local exhaustion for any conversation and model of the account: no
    // upstream call at all.
    let before = Instant::now();
    let local = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("other-conv"),
            OTHER_MODEL_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let after = Instant::now();
    assert_eq!(local.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&local, "x-orqen-error"), Some("1"));
    assert_within(
        retry_after_bounds(deadline, before, after),
        retry_after_secs(&local),
    );
    assert_eq!(json_body(local).await["error"]["type"], "rate_limit_error");
    upstream.assert_idle();
}

#[tokio::test]
async fn attempts_are_bounded_and_upstream_retry_after_kept_while_keys_remain() {
    let keys = [KEY_A, KEY_B, KEY_C];
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &keys).upstream("max_attempts = 2")).await;
    for _ in 0..3 {
        upstream.push(reply(
            StatusCode::TOO_MANY_REQUESTS,
            &[("retry-after", "7")],
            "{}",
        ));
    }
    let response = within(
        post(
            &client(),
            &orqen,
            "/v1/chat/completions",
            Some("conv-bounded"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let first = upstream.next().await;
    let second = upstream.next().await;
    assert_ne!(key_of(&first, &keys), key_of(&second, &keys));
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&response, "x-orqen-attempts"), Some("2"));
    // A third key is still eligible, so the upstream's own value stays.
    assert_eq!(header(&response, "retry-after"), Some("7"));
    upstream.assert_idle();
}

#[tokio::test]
async fn unauthorized_disables_only_that_credential() {
    let keys = [SHARED_1, SHARED_2];
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &keys)).await;
    let client = client();
    upstream.push(reply(
        StatusCode::UNAUTHORIZED,
        &[],
        r#"{"error":"bad key"}"#,
    ));
    upstream.push(ok_json("{}"));
    let response = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-401"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let rejected = key_of(&upstream.next().await, &keys);
    let accepted = key_of(&upstream.next().await, &keys);
    assert_ne!(rejected, accepted, "same-account sibling is still eligible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-orqen-attempts"), Some("2"));
    assert_eq!(header(&response, "x-orqen-key"), Some(accepted));
    upstream.assert_idle();

    for conversation in ["c1", "c2", "c3"] {
        upstream.push(ok_json("{}"));
        let response = within(
            post(
                &client,
                &orqen,
                "/v1/chat/completions",
                Some(conversation),
                CHAT_BODY,
            )
            .send(),
        )
        .await
        .expect("response");
        assert_eq!(key_of(&upstream.next().await, &keys), accepted);
        assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    }
    upstream.assert_idle();
}

#[tokio::test]
async fn payment_required_blocks_the_whole_account() {
    let keys = [SHARED_1, SHARED_2];
    let mut upstream = Upstream::start().await;
    let orqen =
        start(&Setup::new(&upstream.base_url, &keys).limits("default_cooldown_secs = 45")).await;
    let client = client();
    upstream.push(reply(
        StatusCode::PAYMENT_REQUIRED,
        &[],
        r#"{"error":"quota"}"#,
    ));
    let before = Instant::now();
    let response = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("conv-402"),
            CHAT_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let after = Instant::now();
    let _only = upstream.next().await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
    // Recorded and relayed at the same instant: exactly the default cooldown.
    assert_eq!(header(&response, "retry-after"), Some("45"));
    upstream.assert_idle();

    let cooldown = Duration::from_secs(45);
    let query = Instant::now();
    let deadline = query
        + orqen
            .state
            .pool
            .unavailable_for("deepseek-v4.1-flash", query)
            .expect("whole account cooled");
    assert!(
        before + cooldown <= deadline && deadline <= after + cooldown,
        "account deadline starts at the 402 report"
    );

    let before = Instant::now();
    let local = within(
        post(
            &client,
            &orqen,
            "/v1/chat/completions",
            Some("other"),
            OTHER_MODEL_BODY,
        )
        .send(),
    )
    .await
    .expect("response");
    let after = Instant::now();
    assert_eq!(local.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&local, "x-orqen-error"), Some("1"));
    assert_within(
        retry_after_bounds(deadline, before, after),
        retry_after_secs(&local),
    );
    upstream.assert_idle();
}

#[tokio::test]
async fn client_and_server_errors_are_relayed_without_retry() {
    let keys = [KEY_A, KEY_B];
    let mut upstream = Upstream::start().await;
    let orqen = start(&Setup::new(&upstream.base_url, &keys)).await;
    let client = client();
    let body = r#"{"error":{"code":"upstream"}}"#;
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        upstream.push(reply(status, &[("content-type", "application/json")], body));
        let response = within(
            post(
                &client,
                &orqen,
                "/v1/chat/completions",
                Some("conv-errors"),
                CHAT_BODY,
            )
            .send(),
        )
        .await
        .expect("response");
        let _only = upstream.next().await;
        assert_eq!(response.status(), status);
        assert_eq!(header(&response, "x-orqen-attempts"), Some("1"));
        assert_eq!(header(&response, "x-orqen-error"), None);
        assert_eq!(body_text(response).await, body);
        upstream.assert_idle();
    }
}
