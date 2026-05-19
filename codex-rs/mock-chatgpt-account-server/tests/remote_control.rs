//! End-to-end integration tests for the `wham/remote/control/*` relay.
//!
//! These spin up the real mock server on an ephemeral port and exercise the
//! relay surface using off-the-shelf WebSocket and HTTP clients — they do not
//! depend on `codex-app-server-transport`. Each test owns its own server
//! instance so we can run them in parallel without cross-talk.

#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::redundant_clone)]

use std::time::Duration;

use base64::Engine;
use clap::Parser;
use codex_mock_chatgpt_account_server::server::MockServerArgs;
use codex_mock_chatgpt_account_server::server::testing::spawn;
use futures_util::SinkExt;
use futures_util::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;

const BEARER: &str = "Bearer mock-token";
const ACCOUNT: &str = "org-debug";
const INSTALL_HEADER: &str = "x-codex-installation-id";
const SERVER_ID_HEADER: &str = "x-codex-server-id";
const PROTOCOL_HEADER: &str = "x-codex-protocol-version";
const SUBSCRIBE_HEADER: &str = "x-codex-subscribe-cursor";
const ACCOUNT_HEADER: &str = "chatgpt-account-id";
const PROTOCOL_VERSION: &str = "3";

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

struct Harness {
    addr: std::net::SocketAddr,
    _handle: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with_args(MockServerArgs::parse_from(["mock-server"])).await
    }

    async fn start_strict() -> Self {
        Self::start_with_args(MockServerArgs::parse_from([
            "mock-server",
            "--strict-account-header",
        ]))
        .await
    }

    async fn start_with_args(args: MockServerArgs) -> Self {
        let (addr, handle) = spawn(args).await.expect("spawn mock");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("client");
        Self {
            addr,
            _handle: handle,
            client,
        }
    }

    fn enroll_url(&self) -> String {
        format!(
            "http://{}/backend-api/wham/remote/control/server/enroll",
            self.addr
        )
    }

    fn codex_ws_url(&self) -> String {
        format!("ws://{}/backend-api/wham/remote/control/server", self.addr)
    }

    fn phone_ws_url(&self, environment_id: &str, client_id: &str) -> String {
        format!(
            "ws://{}/backend-api/wham/remote/control/client?environment_id={}&client_id={}",
            self.addr, environment_id, client_id,
        )
    }

    fn phone_ws_url_stream(
        &self,
        environment_id: &str,
        client_id: &str,
        stream_id: &str,
    ) -> String {
        format!(
            "ws://{}/backend-api/wham/remote/control/client?environment_id={}&client_id={}&stream_id={}",
            self.addr, environment_id, client_id, stream_id,
        )
    }

    fn phone_ws_url_with(
        &self,
        environment_id: &str,
        client_id: &str,
        extra_query: &str,
    ) -> String {
        format!(
            "ws://{}/backend-api/wham/remote/control/client?environment_id={}&client_id={}&{}",
            self.addr, environment_id, client_id, extra_query,
        )
    }

    async fn enroll(&self, install_id: &str, name: &str) -> reqwest::Response {
        self.client
            .post(self.enroll_url())
            .bearer_auth("mock-token")
            .header(ACCOUNT_HEADER, ACCOUNT)
            .header(INSTALL_HEADER, install_id)
            .json(&json!({
                "name": name,
                "os": "macos",
                "arch": "aarch64",
                "app_server_version": "0.0.0",
                "installation_id": install_id,
            }))
            .send()
            .await
            .expect("enroll send")
    }
}

#[tokio::test]
async fn enroll_happy_path_and_idempotent() {
    let harness = Harness::start().await;
    let first = harness.enroll("install-1", "host-1").await;
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    let first_body: Value = first.json().await.expect("first body");
    let first_server_id = first_body
        .get("server_id")
        .and_then(Value::as_str)
        .expect("server_id")
        .to_string();
    let first_env_id = first_body
        .get("environment_id")
        .and_then(Value::as_str)
        .expect("environment_id")
        .to_string();
    assert!(first_server_id.starts_with("srv_e_"));
    assert!(first_env_id.starts_with("env_"));

    let second = harness.enroll("install-1", "host-1").await;
    let second_body: Value = second.json().await.expect("second body");
    assert_eq!(second_body.get("server_id"), first_body.get("server_id"));
    assert_eq!(
        second_body.get("environment_id"),
        first_body.get("environment_id"),
    );
}

#[tokio::test]
async fn enroll_distinguishes_by_installation_id() {
    let harness = Harness::start().await;
    let first: Value = harness
        .enroll("install-A", "host-1")
        .await
        .json()
        .await
        .expect("first");
    let second: Value = harness
        .enroll("install-B", "host-1")
        .await
        .json()
        .await
        .expect("second");
    assert_ne!(first.get("server_id"), second.get("server_id"));
    assert_ne!(first.get("environment_id"), second.get("environment_id"));
}

#[tokio::test]
async fn enroll_requires_bearer_and_account() {
    let harness = Harness::start_strict().await;
    let no_bearer = harness
        .client
        .post(harness.enroll_url())
        .header(ACCOUNT_HEADER, ACCOUNT)
        .header(INSTALL_HEADER, "i")
        .json(&json!({
            "name": "h",
            "os": "macos",
            "arch": "aarch64",
            "app_server_version": "0.0.0",
            "installation_id": "i",
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(no_bearer.status(), reqwest::StatusCode::UNAUTHORIZED);

    let wrong_account = harness
        .client
        .post(harness.enroll_url())
        .bearer_auth("mock-token")
        .header(ACCOUNT_HEADER, "wrong-account")
        .header(INSTALL_HEADER, "i")
        .json(&json!({
            "name": "h",
            "os": "macos",
            "arch": "aarch64",
            "app_server_version": "0.0.0",
            "installation_id": "i",
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(wrong_account.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn enroll_rejects_install_id_mismatch_between_header_and_body() {
    let harness = Harness::start().await;
    let response = harness
        .client
        .post(harness.enroll_url())
        .bearer_auth("mock-token")
        .header(ACCOUNT_HEADER, ACCOUNT)
        .header(INSTALL_HEADER, "from-header")
        .json(&json!({
            "name": "h",
            "os": "macos",
            "arch": "aarch64",
            "app_server_version": "0.0.0",
            "installation_id": "from-body",
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn codex_ws_rejects_old_protocol_version() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .and_then(Value::as_str)
        .expect("server_id");
    let mut request = harness
        .codex_ws_url()
        .into_client_request()
        .expect("ws request");
    let headers = request.headers_mut();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(BEARER));
    headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(ACCOUNT));
    headers.insert(INSTALL_HEADER, HeaderValue::from_static("install-1"));
    headers.insert(SERVER_ID_HEADER, HeaderValue::from_str(server_id).unwrap());
    headers.insert(PROTOCOL_HEADER, HeaderValue::from_static("2"));
    let result = connect_async(request).await;
    let err = result.expect_err("must fail handshake");
    let message = err.to_string();
    assert!(
        message.contains("426") || message.contains("HTTP error: 426"),
        "expected 426 upgrade required, got: {message}"
    );
}

#[tokio::test]
async fn codex_ws_rejects_unknown_server_id() {
    let harness = Harness::start().await;
    let mut request = harness
        .codex_ws_url()
        .into_client_request()
        .expect("ws request");
    let headers = request.headers_mut();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(BEARER));
    headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(ACCOUNT));
    headers.insert(INSTALL_HEADER, HeaderValue::from_static("install-1"));
    headers.insert(
        SERVER_ID_HEADER,
        HeaderValue::from_static("srv_e_does_not_exist"),
    );
    headers.insert(PROTOCOL_HEADER, HeaderValue::from_static(PROTOCOL_VERSION));
    let err = connect_async(request).await.expect_err("must fail");
    assert!(err.to_string().contains("404"), "expected 404, got: {err}");
}

#[tokio::test]
async fn phone_ws_rejects_unknown_environment_id() {
    let harness = Harness::start().await;
    let mut request = harness
        .phone_ws_url("env_does_not_exist", "client-1")
        .into_client_request()
        .expect("ws request");
    let headers = request.headers_mut();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(BEARER));
    headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(ACCOUNT));
    let err = connect_async(request).await.expect_err("must fail");
    assert!(err.to_string().contains("404"), "expected 404, got: {err}");
}

#[tokio::test]
async fn end_to_end_phone_message_reaches_codex_with_cursor_stamped() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .and_then(Value::as_str)
        .expect("server_id")
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .and_then(Value::as_str)
        .expect("env_id")
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone(&harness, &env_id, "client-1").await;

    let envelope = json!({
        "type": "client_message",
        "message": { "jsonrpc": "2.0", "method": "initialize", "id": 1 },
        "client_id": "client-1",
        "stream_id": "stream-1",
        "seq_id": 1,
    });
    phone
        .send(Message::Text(envelope.to_string().into()))
        .await
        .expect("phone send");

    let received = recv_json(&mut codex).await;
    assert_eq!(
        received.get("type").and_then(Value::as_str),
        Some("client_message"),
    );
    assert_eq!(
        received.get("client_id").and_then(Value::as_str),
        Some("client-1"),
    );
    let cursor = received
        .get("cursor")
        .and_then(Value::as_str)
        .expect("relay stamped cursor")
        .to_string();
    assert_eq!(cursor.len(), 20, "cursor should be 20-digit zero-padded");
    drop(phone);
    drop(codex);
}

#[tokio::test]
async fn end_to_end_codex_response_reaches_phone() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    // Phone primes the relay so codex knows client-1 is online.
    phone
        .send(Message::Text(
            json!({
                "type": "ping",
                "client_id": "client-1",
                "stream_id": "stream-1",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("ping");
    let _ = recv_json(&mut codex).await;

    let server_envelope = json!({
        "type": "server_message",
        "message": { "jsonrpc": "2.0", "result": "ok", "id": 1 },
        "client_id": "client-1",
        "stream_id": "stream-1",
        "seq_id": 1,
    });
    codex
        .send(Message::Text(server_envelope.to_string().into()))
        .await
        .expect("codex send");

    let received = recv_json(&mut phone).await;
    assert_eq!(
        received.get("type").and_then(Value::as_str),
        Some("server_message"),
    );
    assert_eq!(
        received.get("client_id").and_then(Value::as_str),
        Some("client-1"),
    );
    assert!(received.get("cursor").is_some(), "phone should see cursor");
}

#[tokio::test]
async fn codex_reconnect_replays_missed_inbound() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    // Connect codex briefly, then disconnect.
    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone(&harness, &env_id, "client-1").await;

    phone
        .send(Message::Text(
            json!({
                "type": "client_message",
                "message": { "jsonrpc": "2.0", "method": "first", "id": 1 },
                "client_id": "client-1",
                "stream_id": "stream-1",
                "seq_id": 1,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send first");
    let first = recv_json(&mut codex).await;
    let first_cursor = first
        .get("cursor")
        .and_then(Value::as_str)
        .expect("cursor")
        .to_string();

    // Drop codex, send a follow-up while codex is offline.
    drop(codex);
    tokio::time::sleep(Duration::from_millis(50)).await;

    phone
        .send(Message::Text(
            json!({
                "type": "client_message",
                "message": { "jsonrpc": "2.0", "method": "second", "id": 2 },
                "client_id": "client-1",
                "stream_id": "stream-1",
                "seq_id": 2,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send second");
    // Give the relay a moment to record the offline-buffered envelope.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Reconnect codex with subscribe_cursor = first_cursor. Should replay
    // only the second envelope.
    let mut codex2 = connect_codex(&harness, &server_id, "install-1", Some(&first_cursor)).await;
    let replayed = recv_json(&mut codex2).await;
    let payload = replayed
        .get("message")
        .and_then(Value::as_object)
        .expect("message");
    assert_eq!(
        payload.get("method").and_then(Value::as_str),
        Some("second"),
    );
}

#[tokio::test]
async fn phone_reconnect_replays_missed_outbound() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    // Prime the relay so phone is registered.
    phone
        .send(Message::Text(
            json!({
                "type": "ping",
                "client_id": "client-1",
                "stream_id": "stream-1",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("ping");
    let _ = recv_json(&mut codex).await;

    // Codex sends two replies.
    for n in 1..=2 {
        codex
            .send(Message::Text(
                json!({
                    "type": "server_message",
                    "message": { "jsonrpc": "2.0", "result": format!("ok-{n}"), "id": n },
                    "client_id": "client-1",
                    "stream_id": "stream-1",
                    "seq_id": n,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("codex send");
    }

    let first = recv_json(&mut phone).await;
    let first_cursor = first
        .get("cursor")
        .and_then(Value::as_str)
        .expect("cursor")
        .to_string();
    let second = recv_json(&mut phone).await; // consume but don't ack
    drop(phone);
    let _ = second;

    // Reconnect phone with the same stream_id and subscribe_cursor=first_cursor.
    // Replay should yield only the second envelope.
    let resume_query = format!("stream_id=stream-1&subscribe_cursor={first_cursor}");
    let mut phone2 = connect_phone_with(&harness, &env_id, "client-1", &resume_query).await;
    let replayed = recv_json(&mut phone2).await;
    assert_eq!(
        replayed
            .get("message")
            .and_then(|m| m.get("result"))
            .and_then(Value::as_str),
        Some("ok-2"),
    );
}

#[tokio::test]
async fn multi_phone_clients_do_not_cross_talk() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone_a = connect_phone_stream(&harness, &env_id, "client-A", "s-A").await;
    let mut phone_b = connect_phone_stream(&harness, &env_id, "client-B", "s-B").await;

    // Prime both phones with the relay.
    phone_a
        .send(Message::Text(
            json!({"type": "ping", "client_id": "client-A", "stream_id": "s-A"})
                .to_string()
                .into(),
        ))
        .await
        .expect("ping-a");
    phone_b
        .send(Message::Text(
            json!({"type": "ping", "client_id": "client-B", "stream_id": "s-B"})
                .to_string()
                .into(),
        ))
        .await
        .expect("ping-b");
    let _ = recv_json(&mut codex).await;
    let _ = recv_json(&mut codex).await;

    // Codex sends a message addressed to client-A; only phone_a should see it.
    codex
        .send(Message::Text(
            json!({
                "type": "server_message",
                "message": {"jsonrpc": "2.0", "result": "for-a", "id": 1},
                "client_id": "client-A",
                "stream_id": "s-A",
                "seq_id": 1,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("codex send");

    let a_msg = recv_json(&mut phone_a).await;
    assert_eq!(
        a_msg
            .get("message")
            .and_then(|m| m.get("result"))
            .and_then(Value::as_str),
        Some("for-a"),
    );

    // phone_b must not receive anything within a short window.
    match timeout(Duration::from_millis(200), phone_b.next()).await {
        Err(_) => {} // good, no message
        Ok(other) => panic!("phone_b unexpectedly received: {other:?}"),
    }
}

#[tokio::test]
async fn unknown_event_type_is_forwarded_verbatim() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone(&harness, &env_id, "client-1").await;

    phone
        .send(Message::Text(
            json!({
                "type": "experimental_future_event",
                "client_id": "client-1",
                "stream_id": "stream-1",
                "seq_id": 7,
                "payload": {"deep": {"nested": [1, 2, 3]}},
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("phone send");

    let received = recv_json(&mut codex).await;
    assert_eq!(
        received.get("type").and_then(Value::as_str),
        Some("experimental_future_event"),
    );
    assert_eq!(
        received
            .get("payload")
            .and_then(|p| p.pointer("/deep/nested/2")),
        Some(&json!(3)),
    );
}

#[tokio::test]
async fn codex_link_preempted_when_second_codex_connects() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut first = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut second = connect_codex(&harness, &server_id, "install-1", None).await;

    // The first connection should be torn down by the relay.
    match timeout(Duration::from_secs(2), first.next()).await {
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {}
        Ok(Some(Ok(other))) => panic!("expected close on preempted link, got {other:?}"),
        Err(_) => panic!("preempted codex link did not terminate"),
    }
    // The fresh link still operates.
    let _ = second.send(Message::Pong(Vec::new().into())).await;
}

#[tokio::test]
async fn protocol_version_constants_are_three() {
    // Quick sanity check so a future bump of the constant is intentional.
    assert_eq!(PROTOCOL_VERSION, "3");
}

#[tokio::test]
async fn chunked_envelopes_are_forwarded_individually_without_reassembly() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone(&harness, &env_id, "client-1").await;

    let chunk_count: usize = 3;
    for segment_id in 0..chunk_count {
        phone
            .send(Message::Text(
                json!({
                    "type": "client_message_chunk",
                    "client_id": "client-1",
                    "stream_id": "stream-1",
                    "seq_id": 1,
                    "segment_id": segment_id,
                    "segment_count": chunk_count,
                    "message_size_bytes": 300,
                    "message_chunk_base64": "Y2h1bms=", // "chunk"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("phone send");
    }

    // Codex receives 3 envelopes verbatim — the relay does not reassemble.
    for expected_segment in 0..chunk_count {
        let received = recv_json(&mut codex).await;
        assert_eq!(
            received.get("type").and_then(Value::as_str),
            Some("client_message_chunk"),
        );
        assert_eq!(
            received.get("segment_id").and_then(Value::as_u64),
            Some(expected_segment as u64),
        );
    }
}

#[tokio::test]
async fn phone_ack_envelope_is_passed_through_to_codex_unmodified() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone(&harness, &env_id, "client-1").await;

    phone
        .send(Message::Text(
            json!({
                "type": "ack",
                "client_id": "client-1",
                "stream_id": "stream-1",
                "seq_id": 42,
                "segment_id": 7,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("phone send");
    let received = recv_json(&mut codex).await;
    assert_eq!(received.get("type").and_then(Value::as_str), Some("ack"),);
    assert_eq!(received.get("seq_id").and_then(Value::as_u64), Some(42),);
    assert_eq!(received.get("segment_id").and_then(Value::as_u64), Some(7),);
    // The relay still stamps a cursor.
    assert!(received.get("cursor").is_some());
}

#[tokio::test]
async fn concurrent_enroll_same_tuple_yields_one_pair() {
    let harness = Harness::start().await;
    let urls = std::iter::repeat_with(|| harness.enroll_url())
        .take(8)
        .collect::<Vec<_>>();
    let mut tasks = Vec::new();
    for url in urls {
        let client = harness.client.clone();
        tasks.push(tokio::spawn(async move {
            client
                .post(url)
                .bearer_auth("mock-token")
                .header(ACCOUNT_HEADER, ACCOUNT)
                .header(INSTALL_HEADER, "install-1")
                .json(&json!({
                    "name": "host",
                    "os": "macos",
                    "arch": "aarch64",
                    "app_server_version": "0.0.0",
                    "installation_id": "install-1",
                }))
                .send()
                .await
                .expect("enroll send")
                .json::<Value>()
                .await
                .expect("enroll json")
        }));
    }
    let mut bodies = Vec::new();
    for task in tasks {
        bodies.push(task.await.expect("task"));
    }
    let first = bodies[0].clone();
    for body in &bodies[1..] {
        assert_eq!(body.get("server_id"), first.get("server_id"));
        assert_eq!(body.get("environment_id"), first.get("environment_id"));
    }
}

#[tokio::test]
async fn client_closed_drops_phone_link_so_outbound_buffers() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    phone
        .send(Message::Text(
            json!({
                "type": "ping",
                "client_id": "client-1",
                "stream_id": "stream-1",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("ping");
    let _ = recv_json(&mut codex).await;

    phone
        .send(Message::Text(
            json!({
                "type": "client_closed",
                "client_id": "client-1",
                "stream_id": "stream-1",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("phone send closed");

    // Relay forwards ClientClosed to codex.
    let received = recv_json(&mut codex).await;
    assert_eq!(
        received.get("type").and_then(Value::as_str),
        Some("client_closed"),
    );

    // Phone session should terminate; the relay does not require a separate
    // close frame.
    let _ = phone.close(None).await;

    // Codex sends a follow-up addressed to (client-1, stream-1). Since no
    // phone for that stream is attached, the envelope sits in the outbound
    // buffer; a fresh phone subscribing to the same stream replays it.
    codex
        .send(Message::Text(
            json!({
                "type": "server_message",
                "message": {"jsonrpc": "2.0", "result": "after-close", "id": 1},
                "client_id": "client-1",
                "stream_id": "stream-1",
                "seq_id": 99,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("codex send after close");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut phone2 = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;
    let replayed = recv_json(&mut phone2).await;
    assert_eq!(
        replayed
            .get("message")
            .and_then(|m| m.get("result"))
            .and_then(Value::as_str),
        Some("after-close"),
    );
}

// ---------------------------------------------------------------------------
// Protocol-fidelity tests exercising the four review-flagged invariants.
// ---------------------------------------------------------------------------

/// Two parallel WSS streams under the same client_id stay independent: each
/// outbound envelope is delivered only to the phone subscribed to its
/// stream_id, and neither preempts the other.
#[tokio::test]
async fn same_client_id_with_distinct_stream_ids_routes_independently() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone_alpha = connect_phone_stream(&harness, &env_id, "client-1", "stream-alpha").await;
    let mut phone_beta = connect_phone_stream(&harness, &env_id, "client-1", "stream-beta").await;

    // Codex addresses one envelope per stream.
    codex
        .send(Message::Text(
            json!({
                "type": "server_message",
                "message": {"jsonrpc": "2.0", "result": "to-alpha", "id": 1},
                "client_id": "client-1",
                "stream_id": "stream-alpha",
                "seq_id": 1,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("alpha send");
    codex
        .send(Message::Text(
            json!({
                "type": "server_message",
                "message": {"jsonrpc": "2.0", "result": "to-beta", "id": 1},
                "client_id": "client-1",
                "stream_id": "stream-beta",
                "seq_id": 1,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("beta send");

    let alpha = recv_json(&mut phone_alpha).await;
    let beta = recv_json(&mut phone_beta).await;
    assert_eq!(
        alpha
            .get("message")
            .and_then(|m| m.get("result"))
            .and_then(Value::as_str),
        Some("to-alpha"),
    );
    assert_eq!(
        beta.get("message")
            .and_then(|m| m.get("result"))
            .and_then(Value::as_str),
        Some("to-beta"),
    );

    // Confirm no leakage in the other direction.
    match timeout(Duration::from_millis(150), phone_alpha.next()).await {
        Err(_) => {}
        Ok(other) => panic!("phone_alpha unexpectedly received: {other:?}"),
    }
    match timeout(Duration::from_millis(150), phone_beta.next()).await {
        Err(_) => {}
        Ok(other) => panic!("phone_beta unexpectedly received: {other:?}"),
    }
}

/// Reconnecting on the same `(client_id, stream_id)` preempts the prior link
/// with WS close code 4408. A *different* stream_id is *not* preempted.
#[tokio::test]
async fn same_client_and_stream_id_preempts_but_other_stream_unaffected() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut first = connect_phone_stream(&harness, &env_id, "client-1", "stream-X").await;
    let mut neighbor = connect_phone_stream(&harness, &env_id, "client-1", "stream-Y").await;

    // Open a second phone session with the same (client_id, stream_id) — it
    // should preempt `first` but leave `neighbor` alone.
    let _replacement = connect_phone_stream(&harness, &env_id, "client-1", "stream-X").await;

    // `first` should close (the relay emits 4408 with reason).
    match timeout(Duration::from_secs(2), first.next()).await {
        Ok(Some(Ok(Message::Close(Some(frame))))) => {
            assert_eq!(u16::from(frame.code), 4408);
        }
        Ok(Some(Ok(Message::Close(None)))) => {
            // Acceptable: tungstenite may have stripped the code.
        }
        Ok(None) | Ok(Some(Err(_))) => {}
        Ok(Some(Ok(other))) => panic!("expected close on preempted link, got {other:?}"),
        Err(_) => panic!("preempted phone link did not terminate"),
    }

    // `neighbor` should still be alive — sending a ping should not error.
    neighbor
        .send(Message::Text(
            json!({"type": "ping", "client_id": "client-1", "stream_id": "stream-Y"})
                .to_string()
                .into(),
        ))
        .await
        .expect("neighbor still alive");
}

/// Phone `ClientEvent::Ack` trims the relay's outbound buffer; a phone that
/// reconnects after ack'ing every prior envelope does not receive replays.
#[tokio::test]
async fn phone_ack_trims_outbound_replay_buffer() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    // Codex sends two replies.
    for seq in 1..=2 {
        codex
            .send(Message::Text(
                json!({
                    "type": "server_message",
                    "message": {"jsonrpc": "2.0", "result": format!("ok-{seq}"), "id": seq},
                    "client_id": "client-1",
                    "stream_id": "stream-1",
                    "seq_id": seq,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("codex send");
    }
    let first = recv_json(&mut phone).await;
    let second = recv_json(&mut phone).await;
    let first_cursor = first
        .get("cursor")
        .and_then(Value::as_str)
        .expect("cursor")
        .to_string();
    let _ = second;

    // Phone acks seq=2 (= all envelopes processed).
    phone
        .send(Message::Text(
            json!({
                "type": "ack",
                "client_id": "client-1",
                "stream_id": "stream-1",
                "seq_id": 2,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("ack send");

    // Allow the relay to process the ack.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(phone);

    // Reconnect with subscribe_cursor=first_cursor. Because the buffer has
    // been trimmed by the ack, nothing should be replayed for at least 250ms.
    let resume_query = format!("stream_id=stream-1&subscribe_cursor={first_cursor}");
    let mut phone2 = connect_phone_with(&harness, &env_id, "client-1", &resume_query).await;
    match timeout(Duration::from_millis(250), phone2.next()).await {
        Err(_) => {} // good — nothing arrived, buffer was trimmed by ack
        Ok(other) => panic!("ack should have trimmed buffer, but got: {other:?}"),
    }
}

/// Phone-to-Codex messages must persist into the inbound buffer even when
/// Codex is not connected. A Codex that joins later replays every queued
/// envelope in order.
#[tokio::test]
async fn phone_to_codex_persists_while_codex_is_offline() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    // Phone connects first; codex is NOT online yet.
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    // Phone fires 5 messages into the void.
    for n in 1..=5u64 {
        phone
            .send(Message::Text(
                json!({
                    "type": "client_message",
                    "message": {"jsonrpc": "2.0", "method": "while-offline", "id": n},
                    "client_id": "client-1",
                    "stream_id": "stream-1",
                    "seq_id": n,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("phone send");
    }
    // Let the relay finish buffering them.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now codex connects with no subscribe_cursor — should see all 5 in order.
    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        let envelope = recv_json(&mut codex).await;
        let id = envelope
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_u64)
            .expect("id");
        ids.push(id);
    }
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

/// A slow phone whose outbound channel fills up gets closed with WS code
/// 1011. After reconnecting with the appropriate `subscribe_cursor`, the
/// phone observes the messages it couldn't keep up with.
#[tokio::test]
async fn slow_phone_is_kicked_with_close_code_1011() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    // Prime the phone link.
    phone
        .send(Message::Text(
            json!({"type": "ping", "client_id": "client-1", "stream_id": "stream-1"})
                .to_string()
                .into(),
        ))
        .await
        .expect("ping");
    let _ = recv_json(&mut codex).await;

    // Phone is going to be very slow — we hold off reading from it. Codex
    // floods large envelopes so the WSS writer's sink + TCP buffers saturate
    // and the per-phone mpsc channel (PHONE_CHANNEL_CAPACITY = 256) fills.
    // Each payload is ~100 KiB; 600 of them overwhelms any reasonable kernel
    // buffer and drives the relay's `try_send` to `Full` → 1011 kick.
    let filler = "y".repeat(100 * 1024);
    let flood_size: u64 = 600;
    for seq in 1..=flood_size {
        codex
            .send(Message::Text(
                json!({
                    "type": "server_message",
                    "message": {"jsonrpc": "2.0", "result": &filler, "id": seq},
                    "client_id": "client-1",
                    "stream_id": "stream-1",
                    "seq_id": seq,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("codex flood");
    }

    // Drain the phone — eventually it must receive a close 1011.
    let mut saw_close_1011 = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(500), phone.next()).await {
            Err(_) => continue,
            Ok(None) => break,
            Ok(Some(Err(_))) => break,
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                saw_close_1011 = u16::from(frame.code) == 1011;
                break;
            }
            Ok(Some(Ok(Message::Close(None)))) => break,
            Ok(Some(Ok(_))) => {
                // Keep draining — we want to eventually observe the close.
                continue;
            }
        }
    }
    assert!(
        saw_close_1011,
        "expected the phone link to be kicked with WS close code 1011"
    );

    // After reconnecting fresh (no subscribe_cursor, so replay everything),
    // the phone should be able to see at least the most recently buffered
    // envelopes — confirming the buffer survived the kick.
    let mut phone2 = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;
    let next = timeout(Duration::from_secs(5), phone2.next())
        .await
        .expect("post-reconnect recv timeout")
        .expect("stream open")
        .expect("ws ok");
    assert!(
        next.is_text(),
        "expected a buffered envelope after reconnect"
    );
}

/// ClientClosed{stream_id: Some(s)} clears the outbound replay buffer for
/// (client_id, s). A fresh phone subscribing to the same key does NOT
/// observe envelopes the prior session had already received — the closed
/// virtual connection stays closed.
#[tokio::test]
async fn client_closed_clears_outbound_buffer_for_named_stream() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;

    // Codex pushes 3 envelopes for (client-1, stream-1) BEFORE the phone
    // closes the stream. Phone never acks them.
    for seq in 1..=3u64 {
        codex
            .send(Message::Text(
                json!({
                    "type": "server_message",
                    "message": {"jsonrpc": "2.0", "result": format!("pre-close-{seq}"), "id": seq},
                    "client_id": "client-1",
                    "stream_id": "stream-1",
                    "seq_id": seq,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("codex send");
    }
    // Drain the phone so codex_reader actually pushes into the buffer (this
    // also ensures the phone's TCP receive buffer is empty, simulating a
    // normal interactive client that consumed but didn't ack).
    for _ in 0..3 {
        let _ = recv_json(&mut phone).await;
    }

    // Phone closes the stream.
    phone
        .send(Message::Text(
            json!({
                "type": "client_closed",
                "client_id": "client-1",
                "stream_id": "stream-1",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("client_closed send");
    // Wait for the relay to forward to codex and run its cleanup.
    let closed = recv_json(&mut codex).await;
    assert_eq!(
        closed.get("type").and_then(Value::as_str),
        Some("client_closed"),
    );
    drop(phone);
    // Give the relay a moment to finish dropping the stream link.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A fresh phone subscribes to the SAME (client-1, stream-1). Without the
    // close-side cleanup, the replay would resurrect "pre-close-1..3"; with
    // it, the buffer is empty and the read times out.
    let mut phone2 = connect_phone_stream(&harness, &env_id, "client-1", "stream-1").await;
    match timeout(Duration::from_millis(300), phone2.next()).await {
        Err(_) => {} // good — no replay, virtual connection stays closed
        Ok(other) => panic!("client_closed cleanup failed; got: {other:?}"),
    }
}

/// ClientClosed{stream_id: None} clears every outbound buffer entry for the
/// `client_id`. Reconnects on any prior stream of the same client get no
/// replay; only entries Codex pushes after the close are visible.
#[tokio::test]
async fn client_closed_without_stream_id_clears_all_streams_for_client() {
    let harness = Harness::start().await;
    let enrolled: Value = harness
        .enroll("install-1", "host")
        .await
        .json()
        .await
        .expect("enroll");
    let server_id = enrolled
        .get("server_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let env_id = enrolled
        .get("environment_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let mut codex = connect_codex(&harness, &server_id, "install-1", None).await;
    let mut phone_s1 = connect_phone_stream(&harness, &env_id, "client-1", "s1").await;
    let mut phone_s2 = connect_phone_stream(&harness, &env_id, "client-1", "s2").await;

    for &(stream, seq) in &[("s1", 1u64), ("s1", 2), ("s2", 1), ("s2", 2)] {
        codex
            .send(Message::Text(
                json!({
                    "type": "server_message",
                    "message": {"jsonrpc": "2.0", "result": format!("{stream}-{seq}"), "id": seq},
                    "client_id": "client-1",
                    "stream_id": stream,
                    "seq_id": seq,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("codex send");
    }
    // Let the phones drain so the buffer is the only retention.
    for _ in 0..2 {
        let _ = recv_json(&mut phone_s1).await;
    }
    for _ in 0..2 {
        let _ = recv_json(&mut phone_s2).await;
    }

    // Wide ClientClosed (no stream_id). Per protocol, this closes every
    // (client_id, *) stream — buffers included.
    phone_s1
        .send(Message::Text(
            json!({
                "type": "client_closed",
                "client_id": "client-1",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("client_closed send");
    let forwarded = recv_json(&mut codex).await;
    assert_eq!(
        forwarded.get("type").and_then(Value::as_str),
        Some("client_closed"),
    );
    drop(phone_s1);
    drop(phone_s2);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Reconnecting on either of the prior streams should see nothing.
    let mut s1_again = connect_phone_stream(&harness, &env_id, "client-1", "s1").await;
    let mut s2_again = connect_phone_stream(&harness, &env_id, "client-1", "s2").await;
    match timeout(Duration::from_millis(250), s1_again.next()).await {
        Err(_) => {}
        Ok(other) => panic!("s1 replay should be empty after wide close: {other:?}"),
    }
    match timeout(Duration::from_millis(250), s2_again.next()).await {
        Err(_) => {}
        Ok(other) => panic!("s2 replay should be empty after wide close: {other:?}"),
    }
}

async fn connect_codex(
    harness: &Harness,
    server_id: &str,
    installation_id: &str,
    subscribe_cursor: Option<&str>,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = harness
        .codex_ws_url()
        .into_client_request()
        .expect("ws request");
    let headers = request.headers_mut();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(BEARER));
    headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(ACCOUNT));
    headers.insert(
        INSTALL_HEADER,
        HeaderValue::from_str(installation_id).unwrap(),
    );
    headers.insert(SERVER_ID_HEADER, HeaderValue::from_str(server_id).unwrap());
    // The header is informational on the relay side but mirrors how the
    // transport encodes the friendly server name.
    headers.insert(
        "x-codex-name",
        HeaderValue::from_str(
            base64::engine::general_purpose::STANDARD
                .encode("test-host")
                .as_str(),
        )
        .unwrap(),
    );
    headers.insert(PROTOCOL_HEADER, HeaderValue::from_static(PROTOCOL_VERSION));
    if let Some(cursor) = subscribe_cursor {
        headers.insert(SUBSCRIBE_HEADER, HeaderValue::from_str(cursor).unwrap());
    }
    let (stream, _response) = connect_async(request).await.expect("codex ws connect");
    stream
}

async fn connect_phone(
    harness: &Harness,
    environment_id: &str,
    client_id: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    connect_phone_with(harness, environment_id, client_id, "").await
}

async fn connect_phone_stream(
    harness: &Harness,
    environment_id: &str,
    client_id: &str,
    stream_id: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = harness.phone_ws_url_stream(environment_id, client_id, stream_id);
    let mut request = url.into_client_request().expect("phone ws request");
    let headers = request.headers_mut();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(BEARER));
    headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(ACCOUNT));
    let (stream, _response) = connect_async(request).await.expect("phone ws connect");
    stream
}

async fn connect_phone_with(
    harness: &Harness,
    environment_id: &str,
    client_id: &str,
    extra_query: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = if extra_query.is_empty() {
        harness.phone_ws_url(environment_id, client_id)
    } else {
        harness.phone_ws_url_with(environment_id, client_id, extra_query)
    };
    let mut request = url.into_client_request().expect("phone ws request");
    let headers = request.headers_mut();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(BEARER));
    headers.insert(ACCOUNT_HEADER, HeaderValue::from_static(ACCOUNT));
    let (stream, _response) = connect_async(request).await.expect("phone ws connect");
    stream
}

async fn recv_json(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        let next = timeout(RECV_TIMEOUT, stream.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        if next.is_ping() || next.is_pong() {
            continue;
        }
        if let Message::Text(text) = next {
            return serde_json::from_str(text.as_str()).expect("json envelope");
        }
        if matches!(next, Message::Close(_)) {
            panic!("received unexpected close: {next:?}");
        }
    }
}
