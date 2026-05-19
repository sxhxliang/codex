//! Phone side of the relay: `GET /backend-api/wham/remote/control/client`.
//!
//! Phones connect keyed by `environment_id` so they don't need to know
//! Codex's private `server_id`. Each phone session is keyed by
//! `(client_id, stream_id)` — multiple streams under one client_id coexist
//! independently (matches the real transport, where each `(client_id,
//! stream_id)` maps to its own virtual connection in Codex).
//!
//! ## Receive-then-persist
//!
//! Every phone→codex envelope is cursored + pushed onto the relay's inbound
//! buffer *before* it is forwarded to the Codex link. If the Codex link is
//! offline or its inbound channel is full, the envelope is still durable
//! for cursor-based replay when Codex reconnects. A saturated Codex channel
//! triggers a 1011 close on the Codex link (it reconnects and replays).
//!
//! ## Ack-driven trim
//!
//! When the phone sends `ClientEvent::Ack`, the relay both forwards it to
//! Codex (so Codex can trim its own outbound buffer) *and* trims its own
//! outbound buffer for `(client_id, stream_id)` up through
//! `(seq_id, segment_id)`. This stops a reconnecting phone from receiving
//! envelopes it has already acknowledged.

use std::time::Duration;

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::form_urlencoded;
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
use crate::server::remote_control::protocol::REMOTE_CONTROL_SEGMENT_MAX_BYTES;
use crate::server::remote_control::protocol::StreamId;
use crate::server::remote_control::protocol::parse_cursor;
use crate::server::remote_control::protocol::patch_cursor;
use crate::server::remote_control::protocol::peek_envelope;
use crate::server::remote_control::state::CLOSE_CODE_INTERNAL_ERROR;
use crate::server::remote_control::state::CLOSE_CODE_LINK_ROTATED;
use crate::server::remote_control::state::CloseCodeSlot;
use crate::server::remote_control::state::PHONE_CHANNEL_CAPACITY;
use crate::server::remote_control::state::PhoneLink;
use crate::server::remote_control::state::ReplayEntry;
use crate::server::remote_control::with_state;
use crate::server::state::AppState;

/// How long a phone may stay idle (no inbound frames) before the relay drops
/// the link. Lines up with the Codex transport's tracker so both ends time
/// out at the same scale.
const PHONE_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(crate) fn route(state: AppState) -> RemoteControlRoute {
    warp::path!("backend-api" / "wham" / "remote" / "control" / "client")
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
    client_id: ClientId,
    stream_id: StreamId,
    subscribe_cursor: Option<String>,
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
    let Some(environment_id) = params
        .iter()
        .find(|(k, _)| k == "environment_id")
        .map(|(_, v)| v.clone())
    else {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "missing environment_id query parameter" }),
        ));
    };
    let Some(client_id) = params
        .iter()
        .find(|(k, _)| k == "client_id")
        .map(|(_, v)| v.clone())
    else {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "missing client_id query parameter" }),
        ));
    };
    let stream_id = params
        .iter()
        .find(|(k, _)| k == "stream_id")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let subscribe_cursor = params
        .iter()
        .find(|(k, _)| k == "subscribe_cursor")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty());

    let Some(env) = state.relay().resolve_by_env(&environment_id).await else {
        return Err(json_response(
            StatusCode::NOT_FOUND,
            &json!({ "error": format!("unknown environment_id `{environment_id}`") }),
        ));
    };
    if env.account_id != account_id {
        return Err(json_response(
            StatusCode::FORBIDDEN,
            &json!({ "error": "chatgpt-account-id does not match this environment" }),
        ));
    }

    Ok(Handshake {
        server_id: env.server_id,
        client_id: ClientId(client_id),
        stream_id: StreamId(stream_id),
        subscribe_cursor,
    })
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

async fn run_session(socket: WebSocket, state: AppState, handshake: Handshake) {
    let cancel = CancellationToken::new();
    let close_code = CloseCodeSlot::new();
    let (outbound_tx, outbound_rx) = mpsc::channel::<Bytes>(PHONE_CHANNEL_CAPACITY);
    let link = PhoneLink::new(outbound_tx.clone(), cancel.clone(), close_code.clone());
    let generation = link.generation;

    if let Some(prior) = state
        .relay()
        .attach_phone_link(
            &handshake.server_id,
            handshake.client_id.clone(),
            handshake.stream_id.clone(),
            link,
        )
        .await
    {
        prior
            .close_code
            .set(CLOSE_CODE_LINK_ROTATED, "superseded by newer phone link");
        prior.cancel.cancel();
    }

    let (sink, stream) = socket.split();

    let writer = tokio::spawn(phone_writer_loop(
        sink,
        outbound_rx,
        state.clone(),
        WriterContext {
            server_id: handshake.server_id.clone(),
            client_id: handshake.client_id.clone(),
            stream_id: handshake.stream_id.clone(),
            subscribe_cursor: handshake.subscribe_cursor.clone(),
        },
        cancel.clone(),
        close_code.clone(),
    ));
    let reader = tokio::spawn(phone_reader_loop(
        stream,
        state.clone(),
        handshake.server_id.clone(),
        handshake.client_id.clone(),
        handshake.stream_id.clone(),
        cancel.clone(),
    ));

    wait_for_either(writer, reader, cancel.clone()).await;

    state
        .relay()
        .detach_phone_link_if_matching(
            &handshake.server_id,
            &handshake.client_id,
            &handshake.stream_id,
            generation,
        )
        .await;
}

async fn wait_for_either(
    mut writer: JoinHandle<()>,
    mut reader: JoinHandle<()>,
    cancel: CancellationToken,
) {
    tokio::select! {
        _ = &mut writer => {
            cancel.cancel();
            let _ = reader.await;
        }
        _ = &mut reader => {
            cancel.cancel();
            let _ = writer.await;
        }
    }
}

async fn phone_writer_loop(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut outbound_rx: mpsc::Receiver<Bytes>,
    state: AppState,
    ctx: WriterContext,
    cancel: CancellationToken,
    close_code: CloseCodeSlot,
) {
    let WriterContext {
        server_id,
        client_id,
        stream_id,
        subscribe_cursor,
    } = ctx;
    // Replay buffered codex→phone envelopes addressed to *this* stream past
    // the resume cursor. Other streams' backlog is delivered through their
    // own phone sessions; we must not leak them onto this socket.
    let replay = state
        .relay()
        .with_env(&server_id, |entry| {
            entry.buffered_outbound_after_for_stream(
                subscribe_cursor.as_deref(),
                &client_id.0,
                &stream_id.0,
            )
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

    if !cancel.is_cancelled() {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                payload = outbound_rx.recv() => {
                    let Some(payload) = payload else { break };
                    let text = match std::str::from_utf8(&payload) {
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

/// Bundles the per-session parameters the writer needs so the function
/// signature stays under clippy's argument-count threshold.
struct WriterContext {
    server_id: String,
    client_id: ClientId,
    stream_id: StreamId,
    subscribe_cursor: Option<String>,
}

/// Write `msg` to `sink`, but abandon the send if `cancel` fires first. The
/// `sink.send` future is dropped on cancel — this leaves the underlying
/// tokio-tungstenite writer in a defined state (the buffered frame is just
/// discarded). Returns `true` iff the message was actually sent.
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

async fn phone_reader_loop(
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    state: AppState,
    server_id: String,
    client_id: ClientId,
    stream_id: StreamId,
    cancel: CancellationToken,
) {
    loop {
        let next = tokio::time::timeout(PHONE_IDLE_TIMEOUT, stream.next()).await;
        let message = match next {
            Err(_) => {
                warn!("phone session: idle timeout on {}", client_id.0);
                return;
            }
            Ok(None) => return,
            Ok(Some(message)) => message,
        };
        if cancel.is_cancelled() {
            return;
        }
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                warn!("phone session: websocket read error: {err}");
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
            warn!("phone session: dropping non-text frame");
            continue;
        }
        let bytes = message.into_bytes();
        if bytes.len() > REMOTE_CONTROL_SEGMENT_MAX_BYTES {
            warn!(
                "phone session: dropping oversize frame ({} bytes > {})",
                bytes.len(),
                REMOTE_CONTROL_SEGMENT_MAX_BYTES
            );
            continue;
        }

        let Some(routing) = peek_envelope(&bytes) else {
            warn!("phone session: dropping invalid json frame");
            continue;
        };
        let event_type = routing.event_type.clone();
        let envelope_client_id = routing
            .client_id
            .clone()
            .unwrap_or_else(|| client_id.0.clone());
        let envelope_stream_id = routing.stream_id.clone();
        let payload = Bytes::from(bytes);

        // Receive-then-persist + ack trim + ClientClosed cleanup under a
        // single critical section.
        //
        // We always allocate a cursor and push to the inbound buffer before
        // attempting to deliver — that way an offline or backpressured Codex
        // never causes message loss; the buffer carries the envelope until
        // the next Codex reconnect (which replays from cursor > subscribe).
        //
        // If the envelope is `Ack`, trim our own outbound buffer for
        // (client_id, stream_id, seq_id, segment_id). Mirrors the real
        // transport's `BoundedOutboundBuffer::ack` so reconnecting phones
        // don't see already-acknowledged envelopes.
        //
        // If the envelope is `ClientClosed`, drop every outbound buffer
        // entry for the targeted stream (or the whole client when
        // `stream_id` is absent). This prevents a fresh phone re-subscribing
        // to the same `(client_id, stream_id)` from resurrecting the
        // history of a virtual connection the phone has explicitly closed.
        let Some(patched_bytes) = state
            .relay()
            .with_env_mut(&server_id, |entry| {
                // Persist phone→codex envelope.
                let cursor = entry.allocate_cursor();
                let cursor_n = parse_cursor(&cursor).unwrap_or(0);
                let patched = patch_cursor(&payload, &cursor);
                let patched_bytes = Bytes::from(patched);
                entry.push_inbound(ReplayEntry {
                    cursor: cursor_n,
                    client_id: Some(envelope_client_id.clone()),
                    stream_id: envelope_stream_id.clone(),
                    seq_id: routing.seq_id,
                    segment_id: routing.segment_id,
                    payload: patched_bytes.clone(),
                });

                match event_type.as_deref() {
                    Some("ack") => {
                        if let (Some(seq_id), Some(stream_id_str)) =
                            (routing.seq_id, envelope_stream_id.as_deref())
                        {
                            entry.ack_outbound(
                                &envelope_client_id,
                                stream_id_str,
                                seq_id,
                                routing.segment_id,
                            );
                        }
                    }
                    Some("client_closed") => match envelope_stream_id.as_deref() {
                        Some(target_stream) => {
                            entry.drop_outbound_for_stream(&envelope_client_id, target_stream);
                        }
                        None => {
                            entry.drop_outbound_for_client(&envelope_client_id);
                        }
                    },
                    _ => {}
                }

                patched_bytes
            })
            .await
        else {
            return;
        };

        // Best-effort forward to Codex. On backpressure we kick Codex with
        // 1011 — its reconnect-with-cursor will pull everything from the
        // inbound buffer.
        if let Some(codex_handle) = state.relay().codex_link_handle(&server_id).await {
            match codex_handle.inbound_tx.try_send(patched_bytes) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("phone session: codex inbound channel full; kicking codex with 1011");
                    codex_handle
                        .close_code
                        .set(CLOSE_CODE_INTERNAL_ERROR, "codex inbound channel saturated");
                    codex_handle.cancel.cancel();
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Codex link winding down — rely on the inbound buffer.
                }
            }
        }
        // If no Codex link exists, the buffer alone holds the envelope.

        // ClientClosed semantics: explicitly cancel matching phone links.
        //  - With stream_id Some(s): close only (client_id, s). If that's
        //    this session, exit; otherwise also drop the named link.
        //  - With stream_id None: close every (client_id, *) including
        //    this session.
        if event_type.as_deref() == Some("client_closed") {
            let envelope_stream = envelope_stream_id.map(StreamId);
            match envelope_stream {
                Some(target_stream) => {
                    if target_stream == stream_id {
                        // Closing self.
                        return;
                    }
                    // Closing another stream on the same client_id.
                    state
                        .relay()
                        .cancel_phone_link(&server_id, &client_id, &target_stream)
                        .await;
                }
                None => {
                    state
                        .relay()
                        .cancel_all_phone_links_for_client(&server_id, &client_id)
                        .await;
                    return;
                }
            }
        }
    }
}
