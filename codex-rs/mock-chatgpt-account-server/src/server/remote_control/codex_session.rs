//! Codex side of the relay: `GET /backend-api/wham/remote/control/server`.
//!
//! Codex opens a single long-lived WSS per `server_id`. The relay multiplexes
//! many phone clients onto this socket, demuxed by `(client_id, stream_id)`.
//! The handler enforces handshake invariants, replays any inbound envelopes
//! the previous Codex session missed, and runs two background tasks for the
//! lifetime of the connection:
//!
//! - **Writer** — drains the Codex inbound channel (phone→codex bytes have
//!   already been cursored + buffered by `phone_session::phone_reader_loop`)
//!   and writes them onto the socket. On startup it first replays buffered
//!   envelopes whose cursor is strictly greater than the client-provided
//!   `x-codex-subscribe-cursor`.
//! - **Reader** — reads codex→phone bytes off the socket. Each frame is
//!   *persisted first* (cursor allocated, JSON patched, pushed onto the
//!   outbound replay buffer) and *then* forwarded to the matching phone
//!   link by `(client_id, stream_id)`. A slow phone whose outbound channel
//!   has filled is closed with WS code 1011 so it can reconnect with
//!   `subscribe_cursor` and resume from the buffer.

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use warp::Filter;
use warp::http::HeaderMap;
use warp::http::StatusCode;
use warp::reply::Reply;
use warp::reply::Response;
use warp::ws::Message;
use warp::ws::WebSocket;
use warp::ws::Ws;

use crate::server::remote_control::RemoteControlRoute;
use crate::server::remote_control::enroll::has_bearer_with_query_fallback;
use crate::server::remote_control::enroll::header_or_query;
use crate::server::remote_control::enroll::json_response;
use crate::server::remote_control::enroll::unauthorized;
use crate::server::remote_control::protocol::ClientId;
use crate::server::remote_control::protocol::REMOTE_CONTROL_ACCOUNT_ID_HEADER;
use crate::server::remote_control::protocol::REMOTE_CONTROL_INSTALLATION_ID_HEADER;
use crate::server::remote_control::protocol::REMOTE_CONTROL_PROTOCOL_VERSION;
use crate::server::remote_control::protocol::REMOTE_CONTROL_PROTOCOL_VERSION_HEADER;
use crate::server::remote_control::protocol::REMOTE_CONTROL_SEGMENT_MAX_BYTES;
use crate::server::remote_control::protocol::REMOTE_CONTROL_SERVER_ID_HEADER;
use crate::server::remote_control::protocol::REMOTE_CONTROL_SUBSCRIBE_CURSOR_HEADER;
use crate::server::remote_control::protocol::StreamId;
use crate::server::remote_control::protocol::parse_cursor;
use crate::server::remote_control::protocol::patch_cursor;
use crate::server::remote_control::protocol::peek_envelope;
use crate::server::remote_control::state::CLOSE_CODE_INTERNAL_ERROR;
use crate::server::remote_control::state::CLOSE_CODE_LINK_ROTATED;
use crate::server::remote_control::state::CODEX_CHANNEL_CAPACITY;
use crate::server::remote_control::state::CloseCodeSlot;
use crate::server::remote_control::state::CodexLink;
use crate::server::remote_control::state::ReplayEntry;
use crate::server::remote_control::with_state;
use crate::server::state::AppState;

pub(crate) fn route(state: AppState) -> RemoteControlRoute {
    warp::path!("backend-api" / "wham" / "remote" / "control" / "server")
        .and(warp::get())
        .and(warp::ws())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::header::headers_cloned())
        .and(with_state(state))
        .and_then(handle_upgrade)
        .boxed()
}

async fn handle_upgrade(
    ws: Ws,
    query: String,
    headers: HeaderMap,
    state: AppState,
) -> Result<Response, std::convert::Infallible> {
    let handshake = match validate(&state, &query, &headers).await {
        Ok(handshake) => handshake,
        Err(response) => return Ok(response),
    };
    Ok(ws
        .on_upgrade(move |socket| async move {
            run_session(socket, state, handshake).await;
        })
        .into_response())
}

struct Handshake {
    server_id: String,
    /// Highest cursor Codex has already seen. The relay replays inbound
    /// entries strictly greater than this.
    subscribe_cursor: Option<String>,
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

async fn validate(
    state: &AppState,
    query: &str,
    headers: &HeaderMap,
) -> Result<Handshake, Response> {
    let params = parse_query(query);
    if !has_bearer_with_query_fallback(headers, &params) {
        return Err(unauthorized());
    }
    let Some(account_id) = header_or_query(headers, &params, REMOTE_CONTROL_ACCOUNT_ID_HEADER)
    else {
        return Err(unauthorized());
    };
    if state.args.strict_account_header && account_id != state.args.chatgpt_account_id {
        return Err(unauthorized());
    }
    let Some(installation_id) =
        header_or_query(headers, &params, REMOTE_CONTROL_INSTALLATION_ID_HEADER)
    else {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": format!("missing {REMOTE_CONTROL_INSTALLATION_ID_HEADER} header") }),
        ));
    };
    let Some(server_id) = header_or_query(headers, &params, REMOTE_CONTROL_SERVER_ID_HEADER) else {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": format!("missing {REMOTE_CONTROL_SERVER_ID_HEADER} header") }),
        ));
    };
    let protocol_version =
        header_or_query(headers, &params, REMOTE_CONTROL_PROTOCOL_VERSION_HEADER)
            .unwrap_or_default();
    if protocol_version != REMOTE_CONTROL_PROTOCOL_VERSION {
        return Err(json_response(
            StatusCode::UPGRADE_REQUIRED,
            &json!({
                "error": format!(
                    "unsupported protocol version `{protocol_version}`; expected `{REMOTE_CONTROL_PROTOCOL_VERSION}`"
                ),
            }),
        ));
    }
    let Some(env) = state.relay().resolve_by_server(&server_id).await else {
        return Err(json_response(
            StatusCode::NOT_FOUND,
            &json!({ "error": format!("unknown server_id `{server_id}`") }),
        ));
    };
    if env.account_id != account_id {
        return Err(json_response(
            StatusCode::FORBIDDEN,
            &json!({ "error": "chatgpt-account-id does not match this server enrollment" }),
        ));
    }
    if env.installation_id != installation_id {
        return Err(json_response(
            StatusCode::FORBIDDEN,
            &json!({ "error": "x-codex-installation-id does not match this server enrollment" }),
        ));
    }
    let subscribe_cursor =
        header_or_query(headers, &params, REMOTE_CONTROL_SUBSCRIBE_CURSOR_HEADER)
            .filter(|s| !s.is_empty());
    Ok(Handshake {
        server_id,
        subscribe_cursor,
    })
}

async fn run_session(socket: WebSocket, state: AppState, handshake: Handshake) {
    let cancel = CancellationToken::new();
    let close_code = CloseCodeSlot::new();
    let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(CODEX_CHANNEL_CAPACITY);
    let link = CodexLink::new(inbound_tx.clone(), cancel.clone(), close_code.clone());
    let generation = link.generation;

    if let Some(prior) = state
        .relay()
        .attach_codex_link(&handshake.server_id, link)
        .await
    {
        prior
            .close_code
            .set(CLOSE_CODE_LINK_ROTATED, "superseded by newer codex link");
        prior.cancel.cancel();
    }

    let (sink, stream) = socket.split();

    let writer = tokio::spawn(codex_writer_loop(
        sink,
        inbound_rx,
        state.clone(),
        handshake.server_id.clone(),
        handshake.subscribe_cursor.clone(),
        cancel.clone(),
        close_code.clone(),
    ));
    let reader = tokio::spawn(codex_reader_loop(
        stream,
        state.clone(),
        handshake.server_id.clone(),
        cancel.clone(),
    ));

    let _ = wait_for_either(writer, reader, cancel.clone()).await;

    state
        .relay()
        .detach_codex_link_if_matching(&handshake.server_id, generation)
        .await;
}

async fn wait_for_either(
    mut writer: JoinHandle<()>,
    mut reader: JoinHandle<()>,
    cancel: CancellationToken,
) -> Result<(), tokio::task::JoinError> {
    tokio::select! {
        result = &mut writer => {
            cancel.cancel();
            let _ = reader.await;
            result
        }
        result = &mut reader => {
            cancel.cancel();
            let _ = writer.await;
            result
        }
    }
}

async fn codex_writer_loop(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    state: AppState,
    server_id: String,
    subscribe_cursor: Option<String>,
    cancel: CancellationToken,
    close_code: CloseCodeSlot,
) {
    // Replay buffered phone→codex envelopes whose cursor is strictly greater
    // than the resume point Codex advertised. The buffer entries are already
    // cursor-stamped by the phone reader, so this is a straight write loop.
    let replay = state
        .relay()
        .with_env(&server_id, |entry| {
            entry.buffered_inbound_after(subscribe_cursor.as_deref())
        })
        .await
        .unwrap_or_default();
    for (_cursor, payload) in replay {
        if cancel.is_cancelled() {
            break;
        }
        let text = match std::str::from_utf8(&payload) {
            Ok(text) => text.to_string(),
            Err(_) => continue,
        };
        if !send_with_cancel(&mut sink, Message::text(text), &cancel).await {
            break;
        }
    }

    // Steady state: cursor allocation and buffer push happened on the receive
    // side (phone_reader_loop), so the writer only forwards what's already
    // been persisted. This makes Codex disconnect/reconnect lossless even
    // when the channel is mid-flight.
    if !cancel.is_cancelled() {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                raw = inbound_rx.recv() => {
                    let Some(raw) = raw else { break };
                    let text = match std::str::from_utf8(&raw) {
                        Ok(text) => text.to_string(),
                        Err(_) => continue,
                    };
                    if !send_with_cancel(&mut sink, Message::text(text), &cancel).await {
                        break;
                    }
                }
            }
        }
    }

    emit_close(&mut sink, &close_code).await;
}

/// Write `msg` to `sink` but abandon the send if `cancel` fires first.
/// Returns `true` iff the message actually went through.
async fn send_with_cancel(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: Message,
    cancel: &CancellationToken,
) -> bool {
    let send_fut = sink.send(msg);
    tokio::pin!(send_fut);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        result = &mut send_fut => result.is_ok(),
    }
}

async fn emit_close(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    close_code: &CloseCodeSlot,
) {
    if let Some((code, reason)) = close_code.take() {
        let _ = sink.send(Message::close_with(code, reason)).await;
    } else {
        let _ = sink.send(Message::close()).await;
    }
}

async fn codex_reader_loop(
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    state: AppState,
    server_id: String,
    cancel: CancellationToken,
) {
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => return,
            message = stream.next() => match message {
                Some(message) => message,
                None => return,
            },
        };
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                warn!("codex session: websocket read error: {err}");
                return;
            }
        };
        if message.is_close() {
            return;
        }
        if message.is_ping() || message.is_pong() {
            continue;
        }
        if !message.is_text() {
            warn!("codex session: dropping non-text frame");
            continue;
        }
        let bytes = message.into_bytes();
        if bytes.len() > REMOTE_CONTROL_SEGMENT_MAX_BYTES {
            warn!(
                "codex session: dropping oversize frame ({} bytes > {})",
                bytes.len(),
                REMOTE_CONTROL_SEGMENT_MAX_BYTES
            );
            continue;
        }
        let Some(routing) = peek_envelope(&bytes) else {
            warn!("codex session: dropping invalid json frame");
            continue;
        };
        let Some(client_id_text) = routing.client_id.clone() else {
            warn!("codex session: dropping outbound envelope without client_id");
            continue;
        };
        let Some(stream_id_text) = routing.stream_id.clone() else {
            warn!(
                "codex session: dropping outbound envelope without stream_id (client_id={})",
                client_id_text
            );
            continue;
        };
        let client_id = ClientId(client_id_text);
        let stream_id = StreamId(stream_id_text);

        // Receive-then-persist: allocate a cursor + push the patched bytes
        // onto the outbound buffer atomically, BEFORE attempting delivery.
        // This guarantees that even if the phone is offline or its channel
        // is full, the envelope is durable for cursor replay.
        let routing_for_entry = routing.clone();
        let payload_bytes = Bytes::from(bytes);
        let Some(patched_bytes) = state
            .relay()
            .with_env_mut(&server_id, |entry| {
                let cursor = entry.allocate_cursor();
                let patched = patch_cursor(&payload_bytes, &cursor);
                let patched_bytes = Bytes::from(patched);
                let cursor_n = parse_cursor(&cursor).unwrap_or(0);
                entry.push_outbound(ReplayEntry {
                    cursor: cursor_n,
                    client_id: Some(client_id.0.clone()),
                    stream_id: Some(stream_id.0.clone()),
                    seq_id: routing_for_entry.seq_id,
                    segment_id: routing_for_entry.segment_id,
                    payload: patched_bytes.clone(),
                });
                patched_bytes
            })
            .await
        else {
            return;
        };

        // Best-effort delivery to the live phone link. On backpressure
        // (channel full), kick the phone with 1011 so it reconnects and
        // replays from the buffer. If no phone link exists, the buffer
        // alone carries the envelope until a phone subscribes.
        let Some(handle) = state
            .relay()
            .phone_link_handle(&server_id, &client_id, &stream_id)
            .await
        else {
            continue;
        };
        match handle.outbound_tx.try_send(patched_bytes) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    "codex session: phone outbound channel full for ({}, {}); kicking with 1011",
                    client_id.0, stream_id.0
                );
                handle.close_code.set(
                    CLOSE_CODE_INTERNAL_ERROR,
                    "phone outbound channel saturated",
                );
                handle.cancel.cancel();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Phone link is winding down; rely on the buffer for replay
                // on reconnect.
            }
        }
    }
}
