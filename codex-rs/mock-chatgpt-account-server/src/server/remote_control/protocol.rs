//! Wire protocol for `wham/remote/control/*`. Field names and shapes match
//! the types declared in
//! `codex-rs/app-server-transport/src/transport/remote_control/protocol.rs`,
//! but are re-declared here so the mock can compile without depending on the
//! transport crate.
//!
//! The relay only ever inspects routing/discriminator fields, so message
//! payloads (`ServerMessage::message`, `ClientMessage::message`) are typed as
//! `serde_json::Value` and forwarded verbatim — unknown event variants pass
//! through unchanged.

#![allow(dead_code)]

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Advertised in the `x-codex-protocol-version` request header during the
/// Codex WSS handshake. Codex pins this; the relay rejects anything else.
pub(crate) const REMOTE_CONTROL_PROTOCOL_VERSION: &str = "3";

pub(crate) const REMOTE_CONTROL_PROTOCOL_VERSION_HEADER: &str = "x-codex-protocol-version";
pub(crate) const REMOTE_CONTROL_SERVER_ID_HEADER: &str = "x-codex-server-id";
pub(crate) const REMOTE_CONTROL_SERVER_NAME_HEADER: &str = "x-codex-name";
pub(crate) const REMOTE_CONTROL_SUBSCRIBE_CURSOR_HEADER: &str = "x-codex-subscribe-cursor";
pub(crate) const REMOTE_CONTROL_INSTALLATION_ID_HEADER: &str = "x-codex-installation-id";
pub(crate) const REMOTE_CONTROL_ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";

/// Maximum WSS frame size the producer aims for (≈ TCP/TLS-friendly).
pub(crate) const REMOTE_CONTROL_SEGMENT_TARGET_BYTES: usize = 100 * 1024;
/// Hard ceiling the relay enforces per frame. Larger frames get dropped.
pub(crate) const REMOTE_CONTROL_SEGMENT_MAX_BYTES: usize = 150 * 1024;
/// Maximum reassembled message size (relay does not reassemble, just forwards).
#[allow(dead_code)]
pub(crate) const REMOTE_CONTROL_REASSEMBLED_MAX_BYTES: usize = 100 * 1024 * 1024;
/// Maximum number of segments per reassembled message.
#[allow(dead_code)]
pub(crate) const REMOTE_CONTROL_SEGMENT_COUNT_MAX: usize = 1024;

/// Opaque per-remote-client identifier (e.g. one phone instance).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ClientId(pub String);

/// Opaque identifier for a logical session within a client. Codex demuxes
/// `(client_id, stream_id)` into independent virtual connections.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct StreamId(pub String);

// ---------------------------------------------------------------------------
// Enrollment (HTTPS)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnrollRemoteServerRequest {
    pub(crate) name: String,
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) app_server_version: String,
    pub(crate) installation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnrollRemoteServerResponse {
    pub(crate) server_id: String,
    pub(crate) environment_id: String,
}

// ---------------------------------------------------------------------------
// Client → Codex events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClientEvent {
    ClientMessage {
        message: Value,
    },
    ClientMessageChunk {
        segment_id: usize,
        segment_count: usize,
        message_size_bytes: usize,
        message_chunk_base64: String,
    },
    /// Phone-side ack of the highest `ServerEnvelope.seq_id` it has processed
    /// for `(client_id, stream_id)`. The relay does not consume this; it is
    /// transparently forwarded to Codex which trims its outbound buffer.
    Ack {
        #[serde(skip_serializing_if = "Option::is_none")]
        segment_id: Option<usize>,
    },
    Ping,
    ClientClosed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ClientEnvelope {
    #[serde(flatten)]
    pub(crate) event: ClientEvent,
    #[serde(rename = "client_id")]
    pub(crate) client_id: ClientId,
    #[serde(rename = "stream_id", skip_serializing_if = "Option::is_none")]
    pub(crate) stream_id: Option<StreamId>,
    #[serde(rename = "seq_id", skip_serializing_if = "Option::is_none")]
    pub(crate) seq_id: Option<u64>,
    /// Relay-stamped resume token. Codex echoes the highest value seen back
    /// in `x-codex-subscribe-cursor` on reconnect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Codex → Client events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PongStatus {
    Active,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerEvent {
    ServerMessage {
        message: Value,
    },
    ServerMessageChunk {
        segment_id: usize,
        segment_count: usize,
        message_size_bytes: usize,
        message_chunk_base64: String,
    },
    #[allow(dead_code)]
    Ack,
    Pong {
        status: PongStatus,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ServerEnvelope {
    #[serde(flatten)]
    pub(crate) event: ServerEvent,
    #[serde(rename = "client_id")]
    pub(crate) client_id: ClientId,
    #[serde(rename = "stream_id")]
    pub(crate) stream_id: StreamId,
    #[serde(rename = "seq_id")]
    pub(crate) seq_id: u64,
}

/// Routing fields the relay needs from an envelope without committing to a
/// specific variant. `event_type` is the discriminator (the `"type"` field on
/// the wire); the other fields are present only when applicable.
#[derive(Debug, Default, Clone)]
pub(crate) struct EnvelopeRouting {
    pub event_type: Option<String>,
    pub client_id: Option<String>,
    pub stream_id: Option<String>,
    pub seq_id: Option<u64>,
    pub segment_id: Option<usize>,
}

/// Best-effort peek into an envelope's routing fields. The relay must accept
/// unknown event variants and forward them verbatim, so this never fails for
/// well-formed JSON; missing fields just remain `None`.
pub(crate) fn peek_envelope(bytes: &[u8]) -> Option<EnvelopeRouting> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let obj = value.as_object()?;
    Some(EnvelopeRouting {
        event_type: obj
            .get("type")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        client_id: obj
            .get("client_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        stream_id: obj
            .get("stream_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        seq_id: obj.get("seq_id").and_then(Value::as_u64),
        segment_id: obj
            .get("segment_id")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    })
}

/// Stamp a relay-generated `cursor` onto an envelope JSON without
/// deserializing the body. Returns the patched JSON text.
///
/// Falls back to inserting `"cursor":"…"` after the leading `{` when the
/// payload is not a JSON object (the relay forwards malformed messages with
/// best effort).
pub(crate) fn patch_cursor(bytes: &[u8], cursor: &str) -> Vec<u8> {
    if let Ok(mut value) = serde_json::from_slice::<Value>(bytes)
        && let Some(obj) = value.as_object_mut()
    {
        obj.insert("cursor".to_string(), Value::String(cursor.to_string()));
        return serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec());
    }
    bytes.to_vec()
}

/// Format a numeric cursor as a 20-digit zero-padded decimal string so
/// lexicographic comparison agrees with numeric comparison (Codex stores the
/// highest cursor as a `String`).
pub(crate) fn format_cursor(cursor: u64) -> String {
    format!("{cursor:020}")
}

/// Parse a wire-format cursor back to a `u64`. Accepts any decimal,
/// zero-padded or not.
pub(crate) fn parse_cursor(cursor: &str) -> Option<u64> {
    cursor.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn ping_envelope_round_trip_preserves_wire_shape() {
        let envelope = ClientEnvelope {
            event: ClientEvent::Ping,
            client_id: ClientId("c-1".to_string()),
            stream_id: Some(StreamId("s-1".to_string())),
            seq_id: None,
            cursor: None,
        };
        let wire = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(
            wire,
            json!({
                "type": "ping",
                "client_id": "c-1",
                "stream_id": "s-1",
            })
        );
        let back: ClientEnvelope = serde_json::from_value(wire).expect("deserialize");
        assert!(matches!(back.event, ClientEvent::Ping));
        assert_eq!(back.client_id.0, "c-1");
    }

    #[test]
    fn server_pong_envelope_wire_shape() {
        let envelope = ServerEnvelope {
            event: ServerEvent::Pong {
                status: PongStatus::Active,
            },
            client_id: ClientId("c-1".to_string()),
            stream_id: StreamId("s-1".to_string()),
            seq_id: 7,
        };
        let wire = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(
            wire,
            json!({
                "type": "pong",
                "status": "active",
                "client_id": "c-1",
                "stream_id": "s-1",
                "seq_id": 7,
            })
        );
    }

    #[test]
    fn cursor_lexicographic_order_matches_numeric() {
        let mut cursors = (0u64..=15).map(format_cursor).collect::<Vec<_>>();
        let sorted_lex = {
            let mut copy = cursors.clone();
            copy.sort();
            copy
        };
        cursors.sort_by_key(|cursor| parse_cursor(cursor).unwrap());
        assert_eq!(sorted_lex, cursors);
    }

    #[test]
    fn patch_cursor_inserts_field_into_unknown_event() {
        let bytes = serde_json::to_vec(&json!({
            "type": "experimental_future_event",
            "client_id": "c-1",
            "stream_id": "s-1",
            "payload": { "anything": [1, 2, 3] },
        }))
        .expect("serialize");
        let patched = patch_cursor(&bytes, "00000000000000000042");
        let value: Value = serde_json::from_slice(&patched).expect("deserialize patched");
        assert_eq!(
            value.get("cursor").and_then(Value::as_str),
            Some("00000000000000000042")
        );
        assert_eq!(
            value.get("type").and_then(Value::as_str),
            Some("experimental_future_event")
        );
    }

    #[test]
    fn peek_envelope_extracts_routing_fields() {
        let bytes = serde_json::to_vec(&json!({
            "type": "client_message_chunk",
            "client_id": "c-1",
            "stream_id": "s-1",
            "seq_id": 4,
            "segment_id": 2,
        }))
        .expect("serialize");
        let routing = peek_envelope(&bytes).expect("peek");
        assert_eq!(routing.event_type.as_deref(), Some("client_message_chunk"));
        assert_eq!(routing.client_id.as_deref(), Some("c-1"));
        assert_eq!(routing.stream_id.as_deref(), Some("s-1"));
        assert_eq!(routing.seq_id, Some(4));
        assert_eq!(routing.segment_id, Some(2));
    }
}
