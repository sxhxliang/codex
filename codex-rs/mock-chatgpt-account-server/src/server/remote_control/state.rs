//! Long-lived relay state shared by all `/wham/remote/control/*` handlers.
//!
//! State is keyed by `server_id` (Codex's stable enrollment id) and indexed
//! by `environment_id` (the routing key the phone uses). Each environment
//! tracks at most one Codex WSS link plus a fan-out registry of phone links;
//! buffers in either direction carry envelopes the relay has already accepted
//! but not yet handed to the other side.
//!
//! ## Replay model
//!
//! - Inbound (phone → codex) and outbound (codex → phone) replay buffers are
//!   *receive-then-persist*: the reader task allocates a cursor under the env
//!   lock, patches the envelope, pushes the patched bytes onto the buffer,
//!   and only then tries to forward via the live channel. If the peer is
//!   offline or its channel is full, the message is still durably buffered
//!   for cursor-based replay on reconnect.
//! - The outbound buffer is *acked* by phone `Ack` envelopes: the relay
//!   trims entries for `(client_id, stream_id)` whose `(seq_id, segment_id)`
//!   tuple has been acknowledged, so reconnecting phones don't re-receive
//!   already-processed envelopes.
//! - The inbound buffer is trimmed only by the per-direction size cap; Codex
//!   does not ack inbound traffic at the relay layer.

#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::server::remote_control::protocol::ClientId;
use crate::server::remote_control::protocol::StreamId;
use crate::server::remote_control::protocol::format_cursor;
use crate::server::remote_control::protocol::parse_cursor;

/// Bound on each direction's replay buffer. The relay drops the oldest entry
/// when adding a new one would exceed this size.
pub(crate) const REPLAY_BUFFER_MAX_BYTES: usize = 10 * 1024 * 1024;

/// Maximum number of pending outbound envelopes queued per phone before the
/// relay treats the phone as a slow consumer and closes its WSS with 1011.
pub(crate) const PHONE_CHANNEL_CAPACITY: usize = 256;
/// Maximum number of pending inbound envelopes queued for Codex. Codex is
/// expected to be a fast consumer but we still bound it; a full channel
/// triggers a 1011 close so Codex reconnects and replays from the buffer.
pub(crate) const CODEX_CHANNEL_CAPACITY: usize = 1024;

/// WebSocket close code used by the relay when it kicks a slow consumer.
/// Matches RFC 6455 1011 ("internal error"); phones treat it as a signal to
/// reconnect with `subscribe_cursor` to resume from the replay buffer.
pub(crate) const CLOSE_CODE_INTERNAL_ERROR: u16 = 1011;
/// Close code returned to a Codex link superseded by a fresh enrollment of
/// the same server_id. The transport treats 4408 as an explicit "rotate"
/// directive (mirrors the real ChatGPT backend behavior).
pub(crate) const CLOSE_CODE_LINK_ROTATED: u16 = 4408;

/// Idempotency key for `enroll`. Two requests with identical key must return
/// identical `(server_id, environment_id)` pairs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct EnrollKey {
    pub account_id: String,
    pub installation_id: String,
    pub name: String,
}

/// A buffered envelope alongside the routing metadata the relay needs to:
/// (a) replay only entries addressed to a particular phone session, and
/// (b) trim entries when the phone acknowledges them.
#[derive(Debug, Clone)]
pub(crate) struct ReplayEntry {
    pub cursor: u64,
    pub client_id: Option<String>,
    pub stream_id: Option<String>,
    pub seq_id: Option<u64>,
    pub segment_id: Option<usize>,
    pub payload: Bytes,
}

/// Bounded FIFO of `ReplayEntry`. Drops the oldest entries until adding the
/// incoming one keeps total bytes ≤ `cap`. Supports range queries by cursor
/// and selective trim by `(client_id, stream_id, seq_id, segment_id)` ack.
pub(crate) struct ReplayBuffer {
    entries: VecDeque<ReplayEntry>,
    total_bytes: usize,
    cap: usize,
}

impl ReplayBuffer {
    fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            cap,
        }
    }

    fn push(&mut self, entry: ReplayEntry) {
        let payload_size = entry.payload.len();
        self.entries.push_back(entry);
        self.total_bytes = self.total_bytes.saturating_add(payload_size);
        while self.total_bytes > self.cap {
            let Some(dropped) = self.entries.pop_front() else {
                self.total_bytes = 0;
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(dropped.payload.len());
        }
    }

    /// Entries with `cursor > after` in FIFO order. Used by Codex reconnect:
    /// `subscribe_cursor` is global per env, so no client/stream filter.
    fn entries_after(&self, after: Option<u64>) -> Vec<(u64, Bytes)> {
        self.entries
            .iter()
            .filter(|entry| match after {
                Some(threshold) => entry.cursor > threshold,
                None => true,
            })
            .map(|entry| (entry.cursor, entry.payload.clone()))
            .collect()
    }

    /// Entries with `cursor > after` whose `(client_id, stream_id)` matches.
    /// Used by phone reconnect — each `(client_id, stream_id)` is an
    /// independent logical session, so phones only see their own backlog.
    fn entries_after_for_stream(
        &self,
        after: Option<u64>,
        client_id: &str,
        stream_id: &str,
    ) -> Vec<(u64, Bytes)> {
        self.entries
            .iter()
            .filter(|entry| match after {
                Some(threshold) => entry.cursor > threshold,
                None => true,
            })
            .filter(|entry| {
                entry.client_id.as_deref() == Some(client_id)
                    && entry.stream_id.as_deref() == Some(stream_id)
            })
            .map(|entry| (entry.cursor, entry.payload.clone()))
            .collect()
    }

    /// Mirror of `BoundedOutboundBuffer::ack` in the real transport: drop
    /// every entry for `(client_id, stream_id)` whose
    /// `(seq_id, segment_id_or_zero)` is ≤ `(acked_seq, acked_seg_or_max)`.
    /// Returns the number of entries dropped, for diagnostics.
    fn ack(
        &mut self,
        client_id: &str,
        stream_id: &str,
        acked_seq_id: u64,
        acked_segment_id: Option<usize>,
    ) -> usize {
        let acked_cursor = (acked_seq_id, acked_segment_id.unwrap_or(usize::MAX));
        let mut total_bytes = self.total_bytes;
        let mut trimmed = 0usize;
        self.entries.retain(|entry| {
            let matches_stream = entry.client_id.as_deref() == Some(client_id)
                && entry.stream_id.as_deref() == Some(stream_id);
            if !matches_stream {
                return true;
            }
            let Some(their_seq) = entry.seq_id else {
                return true;
            };
            let their_segment = entry.segment_id.unwrap_or(0);
            let envelope_cursor = (their_seq, their_segment);
            let is_acked = envelope_cursor <= acked_cursor;
            if is_acked {
                total_bytes = total_bytes.saturating_sub(entry.payload.len());
                trimmed += 1;
            }
            !is_acked
        });
        self.total_bytes = total_bytes;
        trimmed
    }

    /// Drop every entry for `(client_id, stream_id)`. Called when a phone
    /// closes a stream so the next reconnect for that stream starts clean.
    fn drop_stream(&mut self, client_id: &str, stream_id: &str) -> usize {
        let mut total_bytes = self.total_bytes;
        let mut trimmed = 0usize;
        self.entries.retain(|entry| {
            let matches = entry.client_id.as_deref() == Some(client_id)
                && entry.stream_id.as_deref() == Some(stream_id);
            if matches {
                total_bytes = total_bytes.saturating_sub(entry.payload.len());
                trimmed += 1;
            }
            !matches
        });
        self.total_bytes = total_bytes;
        trimmed
    }

    /// Drop every entry for `client_id` regardless of stream. Used when the
    /// phone sends `ClientClosed` without a `stream_id`.
    fn drop_client(&mut self, client_id: &str) -> usize {
        let mut total_bytes = self.total_bytes;
        let mut trimmed = 0usize;
        self.entries.retain(|entry| {
            let matches = entry.client_id.as_deref() == Some(client_id);
            if matches {
                total_bytes = total_bytes.saturating_sub(entry.payload.len());
                trimmed += 1;
            }
            !matches
        });
        self.total_bytes = total_bytes;
        trimmed
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// Monotonic generation counter shared across the whole relay. Each link
/// (Codex or phone) receives a unique generation at attach time so the
/// detach path can confirm it is removing *its own* link, not a successor.
static NEXT_LINK_GENERATION: AtomicU64 = AtomicU64::new(1);

fn fresh_link_generation() -> u64 {
    NEXT_LINK_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Side-channel a link uses to tell the WSS writer task which close code to
/// send on shutdown. The relay stamps a code (e.g. 1011) and then cancels
/// the link's cancellation token; the writer reads the stamp on cancel and
/// emits the corresponding close frame. Set-once semantics: the first
/// stamper wins.
#[derive(Clone, Default)]
pub(crate) struct CloseCodeSlot(Arc<StdMutex<Option<(u16, String)>>>);

impl CloseCodeSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, code: u16, reason: impl Into<String>) {
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            *guard = Some((code, reason.into()));
        }
    }

    pub fn take(&self) -> Option<(u16, String)> {
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.take()
    }
}

/// Active phone WSS link, keyed by `(client_id, stream_id)`. Multiple
/// streams under the same `client_id` are independent: each gets its own
/// `PhoneLink` and they coexist.
pub(crate) struct PhoneLink {
    pub generation: u64,
    pub outbound_tx: mpsc::Sender<Bytes>,
    pub cancel: CancellationToken,
    pub close_code: CloseCodeSlot,
}

/// Active Codex WSS link. Codex side is single-tenant per environment; a
/// new connection preempts the prior one.
pub(crate) struct CodexLink {
    pub generation: u64,
    pub inbound_tx: mpsc::Sender<Bytes>,
    pub cancel: CancellationToken,
    pub close_code: CloseCodeSlot,
}

impl PhoneLink {
    pub(crate) fn new(
        outbound_tx: mpsc::Sender<Bytes>,
        cancel: CancellationToken,
        close_code: CloseCodeSlot,
    ) -> Self {
        Self {
            generation: fresh_link_generation(),
            outbound_tx,
            cancel,
            close_code,
        }
    }
}

impl CodexLink {
    pub(crate) fn new(
        inbound_tx: mpsc::Sender<Bytes>,
        cancel: CancellationToken,
        close_code: CloseCodeSlot,
    ) -> Self {
        Self {
            generation: fresh_link_generation(),
            inbound_tx,
            cancel,
            close_code,
        }
    }
}

/// Snapshot of a phone link's "wake" handles, returned by lookups so callers
/// can deliver bytes or kick the link without holding the relay lock.
#[derive(Clone)]
pub(crate) struct PhoneLinkHandle {
    pub outbound_tx: mpsc::Sender<Bytes>,
    pub cancel: CancellationToken,
    pub close_code: CloseCodeSlot,
}

/// Snapshot of a Codex link's "wake" handles, mirroring `PhoneLinkHandle`.
#[derive(Clone)]
pub(crate) struct CodexLinkHandle {
    pub inbound_tx: mpsc::Sender<Bytes>,
    pub cancel: CancellationToken,
    pub close_code: CloseCodeSlot,
}

pub(crate) struct EnvironmentEntry {
    pub account_id: String,
    pub installation_id: String,
    pub server_id: String,
    pub environment_id: String,
    pub name: String,
    pub codex_link: Option<CodexLink>,
    /// Keyed by `(client_id, stream_id)` — each logical session is independent.
    /// Multiple streams under the same `client_id` coexist.
    pub phone_links: HashMap<(ClientId, StreamId), PhoneLink>,
    /// phone → codex envelopes pending Codex receipt (used for cursor replay
    /// when Codex reconnects with `x-codex-subscribe-cursor`).
    inbound_buffer: ReplayBuffer,
    /// codex → phone envelopes pending phone receipt. Trimmed both by size
    /// cap and by phone `Ack` envelopes (per-stream).
    outbound_buffer: ReplayBuffer,
    /// Monotonic per-environment cursor stamped onto every envelope crossing
    /// the relay. Allocation, patching and buffer push happen under a single
    /// `with_env_mut` critical section so the cursor and the buffered bytes
    /// remain in lockstep even under concurrent senders.
    next_cursor: u64,
}

impl EnvironmentEntry {
    fn new(
        server_id: String,
        environment_id: String,
        account_id: String,
        installation_id: String,
        name: String,
    ) -> Self {
        Self {
            account_id,
            installation_id,
            server_id,
            environment_id,
            name,
            codex_link: None,
            phone_links: HashMap::new(),
            inbound_buffer: ReplayBuffer::new(REPLAY_BUFFER_MAX_BYTES),
            outbound_buffer: ReplayBuffer::new(REPLAY_BUFFER_MAX_BYTES),
            next_cursor: 1,
        }
    }

    pub(crate) fn buffered_inbound_after(&self, cursor: Option<&str>) -> Vec<(u64, Bytes)> {
        let after = cursor.and_then(parse_cursor);
        self.inbound_buffer.entries_after(after)
    }

    pub(crate) fn buffered_outbound_after_for_stream(
        &self,
        cursor: Option<&str>,
        client_id: &str,
        stream_id: &str,
    ) -> Vec<(u64, Bytes)> {
        let after = cursor.and_then(parse_cursor);
        self.outbound_buffer
            .entries_after_for_stream(after, client_id, stream_id)
    }

    pub(crate) fn inbound_len(&self) -> usize {
        self.inbound_buffer.len()
    }

    pub(crate) fn outbound_len(&self) -> usize {
        self.outbound_buffer.len()
    }

    pub(crate) fn inbound_total_bytes(&self) -> usize {
        self.inbound_buffer.total_bytes()
    }

    pub(crate) fn outbound_total_bytes(&self) -> usize {
        self.outbound_buffer.total_bytes()
    }

    pub(crate) fn allocate_cursor(&mut self) -> String {
        let cursor = self.next_cursor;
        self.next_cursor = self.next_cursor.saturating_add(1);
        format_cursor(cursor)
    }

    pub(crate) fn push_inbound(&mut self, entry: ReplayEntry) {
        self.inbound_buffer.push(entry);
    }

    pub(crate) fn push_outbound(&mut self, entry: ReplayEntry) {
        self.outbound_buffer.push(entry);
    }

    /// Trim outbound entries acked by the phone for `(client_id, stream_id)`.
    /// Returns the count trimmed for tests/diagnostics.
    pub(crate) fn ack_outbound(
        &mut self,
        client_id: &str,
        stream_id: &str,
        acked_seq_id: u64,
        acked_segment_id: Option<usize>,
    ) -> usize {
        self.outbound_buffer
            .ack(client_id, stream_id, acked_seq_id, acked_segment_id)
    }

    /// Drop outbound entries for `(client_id, stream_id)`. Called when the
    /// phone closes a single stream.
    pub(crate) fn drop_outbound_for_stream(&mut self, client_id: &str, stream_id: &str) -> usize {
        self.outbound_buffer.drop_stream(client_id, stream_id)
    }

    /// Drop outbound entries for every stream of `client_id`. Called when
    /// the phone closes the client without specifying a stream_id.
    pub(crate) fn drop_outbound_for_client(&mut self, client_id: &str) -> usize {
        self.outbound_buffer.drop_client(client_id)
    }
}

#[derive(Default)]
struct RelayInner {
    /// Idempotency map; same key always resolves to the same env entry.
    by_enroll_key: HashMap<EnrollKey, String>,
    /// Primary store keyed by `server_id`.
    by_server: HashMap<String, EnvironmentEntry>,
    /// Routing index keyed by `environment_id` (used by phones).
    by_env: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub(crate) struct RelayState {
    inner: Arc<Mutex<RelayInner>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct EnrolledEnvironment {
    pub server_id: String,
    pub environment_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedEnvironment {
    pub server_id: String,
    pub environment_id: String,
    pub account_id: String,
    pub installation_id: String,
    pub name: String,
}

impl RelayState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Insert (or look up) the environment for `key`. Returns the
    /// `(server_id, environment_id)` pair; repeat calls with the same key
    /// return the same pair.
    pub(crate) async fn enroll(&self, key: EnrollKey, name: String) -> EnrolledEnvironment {
        let mut inner = self.inner.lock().await;
        if let Some(server_id) = inner.by_enroll_key.get(&key).cloned()
            && let Some(entry) = inner.by_server.get(&server_id)
        {
            return EnrolledEnvironment {
                server_id: entry.server_id.clone(),
                environment_id: entry.environment_id.clone(),
            };
        }
        let server_id = format!("srv_e_{}", Uuid::new_v4().simple());
        let environment_id = format!("env_{}", Uuid::new_v4().simple());
        let entry = EnvironmentEntry::new(
            server_id.clone(),
            environment_id.clone(),
            key.account_id.clone(),
            key.installation_id.clone(),
            name,
        );
        inner.by_enroll_key.insert(key, server_id.clone());
        inner
            .by_env
            .insert(environment_id.clone(), server_id.clone());
        inner.by_server.insert(server_id.clone(), entry);
        EnrolledEnvironment {
            server_id,
            environment_id,
        }
    }

    pub(crate) async fn resolve_by_server(&self, server_id: &str) -> Option<ResolvedEnvironment> {
        let inner = self.inner.lock().await;
        inner
            .by_server
            .get(server_id)
            .map(|entry| ResolvedEnvironment {
                server_id: entry.server_id.clone(),
                environment_id: entry.environment_id.clone(),
                account_id: entry.account_id.clone(),
                installation_id: entry.installation_id.clone(),
                name: entry.name.clone(),
            })
    }

    pub(crate) async fn resolve_by_env(&self, environment_id: &str) -> Option<ResolvedEnvironment> {
        let inner = self.inner.lock().await;
        let server_id = inner.by_env.get(environment_id)?.clone();
        inner
            .by_server
            .get(&server_id)
            .map(|entry| ResolvedEnvironment {
                server_id: entry.server_id.clone(),
                environment_id: entry.environment_id.clone(),
                account_id: entry.account_id.clone(),
                installation_id: entry.installation_id.clone(),
                name: entry.name.clone(),
            })
    }

    pub(crate) async fn with_env_mut<R, F>(&self, server_id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut EnvironmentEntry) -> R,
    {
        let mut inner = self.inner.lock().await;
        let entry = inner.by_server.get_mut(server_id)?;
        Some(f(entry))
    }

    pub(crate) async fn with_env<R, F>(&self, server_id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&EnvironmentEntry) -> R,
    {
        let inner = self.inner.lock().await;
        inner.by_server.get(server_id).map(f)
    }

    /// Attach a Codex session. Returns the previously attached link (if any)
    /// so the caller can cancel it.
    pub(crate) async fn attach_codex_link(
        &self,
        server_id: &str,
        link: CodexLink,
    ) -> Option<CodexLink> {
        let mut inner = self.inner.lock().await;
        let entry = inner.by_server.get_mut(server_id)?;
        entry.codex_link.replace(link)
    }

    /// Detach the Codex session if it still matches `generation`.
    pub(crate) async fn detach_codex_link_if_matching(
        &self,
        server_id: &str,
        generation: u64,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.by_server.get_mut(server_id) else {
            return false;
        };
        if entry
            .codex_link
            .as_ref()
            .map(|link| link.generation == generation)
            .unwrap_or(false)
        {
            entry.codex_link = None;
            true
        } else {
            false
        }
    }

    /// Snapshot of the Codex link's delivery handles, or `None` if no link
    /// is currently attached.
    pub(crate) async fn codex_link_handle(&self, server_id: &str) -> Option<CodexLinkHandle> {
        let inner = self.inner.lock().await;
        inner
            .by_server
            .get(server_id)
            .and_then(|entry| entry.codex_link.as_ref())
            .map(|link| CodexLinkHandle {
                inbound_tx: link.inbound_tx.clone(),
                cancel: link.cancel.clone(),
                close_code: link.close_code.clone(),
            })
    }

    /// Kick the current Codex link with `code` (typically 1011). The writer
    /// task observes the close code on cancellation and emits the close frame.
    pub(crate) async fn kick_codex(&self, server_id: &str, code: u16, reason: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.by_server.get_mut(server_id)
            && let Some(link) = &entry.codex_link
        {
            link.close_code.set(code, reason);
            link.cancel.cancel();
        }
    }

    /// Attach (or replace) a phone link for `(server_id, client_id, stream_id)`.
    /// Returns the previously attached link so the caller can cancel it.
    pub(crate) async fn attach_phone_link(
        &self,
        server_id: &str,
        client_id: ClientId,
        stream_id: StreamId,
        link: PhoneLink,
    ) -> Option<PhoneLink> {
        let mut inner = self.inner.lock().await;
        let entry = inner.by_server.get_mut(server_id)?;
        entry.phone_links.insert((client_id, stream_id), link)
    }

    /// Drop a phone link if it still matches `generation`.
    pub(crate) async fn detach_phone_link_if_matching(
        &self,
        server_id: &str,
        client_id: &ClientId,
        stream_id: &StreamId,
        generation: u64,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.by_server.get_mut(server_id) else {
            return false;
        };
        let key = (client_id.clone(), stream_id.clone());
        let matches = entry
            .phone_links
            .get(&key)
            .map(|link| link.generation == generation)
            .unwrap_or(false);
        if matches {
            entry.phone_links.remove(&key);
            true
        } else {
            false
        }
    }

    /// Drop and cancel every phone link whose key starts with `client_id`,
    /// regardless of stream_id. Used when a phone sends `ClientClosed`
    /// without a `stream_id`. Returns the canceled links so the caller can
    /// observe them (e.g. for tests).
    pub(crate) async fn cancel_all_phone_links_for_client(
        &self,
        server_id: &str,
        client_id: &ClientId,
    ) -> Vec<(ClientId, StreamId)> {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.by_server.get_mut(server_id) else {
            return Vec::new();
        };
        let keys: Vec<(ClientId, StreamId)> = entry
            .phone_links
            .keys()
            .filter(|(cid, _)| cid == client_id)
            .cloned()
            .collect();
        for key in &keys {
            if let Some(link) = entry.phone_links.remove(key) {
                link.cancel.cancel();
            }
        }
        keys
    }

    /// Cancel a phone link. Used by ClientClosed{stream_id:Some}. Returns
    /// true if the link was found and canceled.
    pub(crate) async fn cancel_phone_link(
        &self,
        server_id: &str,
        client_id: &ClientId,
        stream_id: &StreamId,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.by_server.get_mut(server_id) else {
            return false;
        };
        let key = (client_id.clone(), stream_id.clone());
        if let Some(link) = entry.phone_links.remove(&key) {
            link.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Snapshot of the phone link's delivery handles for the given key, or
    /// `None` if no phone is currently attached.
    pub(crate) async fn phone_link_handle(
        &self,
        server_id: &str,
        client_id: &ClientId,
        stream_id: &StreamId,
    ) -> Option<PhoneLinkHandle> {
        let inner = self.inner.lock().await;
        inner
            .by_server
            .get(server_id)
            .and_then(|entry| {
                entry
                    .phone_links
                    .get(&(client_id.clone(), stream_id.clone()))
            })
            .map(|link| PhoneLinkHandle {
                outbound_tx: link.outbound_tx.clone(),
                cancel: link.cancel.clone(),
                close_code: link.close_code.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn key(account: &str, install: &str, name: &str) -> EnrollKey {
        EnrollKey {
            account_id: account.to_string(),
            installation_id: install.to_string(),
            name: name.to_string(),
        }
    }

    fn entry(
        cursor: u64,
        client_id: &str,
        stream_id: &str,
        seq_id: Option<u64>,
        segment_id: Option<usize>,
        payload: &str,
    ) -> ReplayEntry {
        ReplayEntry {
            cursor,
            client_id: Some(client_id.to_string()),
            stream_id: Some(stream_id.to_string()),
            seq_id,
            segment_id,
            payload: Bytes::copy_from_slice(payload.as_bytes()),
        }
    }

    #[tokio::test]
    async fn enroll_is_idempotent_for_same_key() {
        let state = RelayState::new();
        let first = state
            .enroll(key("acc-1", "install-1", "host-1"), "host-1".to_string())
            .await;
        let second = state
            .enroll(key("acc-1", "install-1", "host-1"), "host-1".to_string())
            .await;
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn enroll_distinguishes_different_installation_ids() {
        let state = RelayState::new();
        let first = state
            .enroll(key("acc-1", "install-A", "host-1"), "host-1".to_string())
            .await;
        let second = state
            .enroll(key("acc-1", "install-B", "host-1"), "host-1".to_string())
            .await;
        assert_ne!(first.server_id, second.server_id);
        assert_ne!(first.environment_id, second.environment_id);
    }

    #[tokio::test]
    async fn allocate_cursor_is_monotonic() {
        let state = RelayState::new();
        let enrolled = state
            .enroll(key("acc", "install", "host"), "host".to_string())
            .await;
        let first = state
            .with_env_mut(&enrolled.server_id, EnvironmentEntry::allocate_cursor)
            .await
            .expect("env present");
        let second = state
            .with_env_mut(&enrolled.server_id, EnvironmentEntry::allocate_cursor)
            .await
            .expect("env present");
        assert!(parse_cursor(&second).unwrap() > parse_cursor(&first).unwrap());
        assert_eq!(first.len(), 20);
    }

    #[tokio::test]
    async fn replay_after_cursor_filters_in_order() {
        let state = RelayState::new();
        let enrolled = state
            .enroll(key("acc", "install", "host"), "host".to_string())
            .await;
        let mut cursors = Vec::new();
        for n in 1..=5 {
            let cursor = state
                .with_env_mut(&enrolled.server_id, |env_entry| {
                    let cursor = env_entry.allocate_cursor();
                    env_entry.push_inbound(entry(
                        parse_cursor(&cursor).unwrap_or(0),
                        "client",
                        "stream",
                        Some(n as u64),
                        None,
                        &format!("payload-{n}"),
                    ));
                    cursor
                })
                .await
                .expect("env present");
            cursors.push(cursor);
        }
        let after = cursors[1].clone();
        let replayed = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry.buffered_inbound_after(Some(after.as_str()))
            })
            .await
            .expect("env present");
        let returned_cursors = replayed
            .iter()
            .map(|(cursor, _)| format_cursor(*cursor))
            .collect::<Vec<_>>();
        assert_eq!(returned_cursors, cursors[2..]);
    }

    #[tokio::test]
    async fn ack_trims_only_matching_stream() {
        let state = RelayState::new();
        let enrolled = state
            .enroll(key("acc", "install", "host"), "host".to_string())
            .await;
        // 4 outbound envelopes: two for (c1, s1) and two for (c1, s2).
        state
            .with_env_mut(&enrolled.server_id, |env_entry| {
                for &(client, stream, seq) in &[
                    ("c1", "s1", 1u64),
                    ("c1", "s1", 2),
                    ("c1", "s2", 1),
                    ("c1", "s2", 2),
                ] {
                    let cursor = env_entry.allocate_cursor();
                    env_entry.push_outbound(entry(
                        parse_cursor(&cursor).unwrap_or(0),
                        client,
                        stream,
                        Some(seq),
                        None,
                        "x",
                    ));
                }
            })
            .await;

        // Ack (c1, s1) up to seq=2 → trim 2 entries for that stream only.
        let trimmed = state
            .with_env_mut(&enrolled.server_id, |env_entry| {
                env_entry.ack_outbound("c1", "s1", 2, None)
            })
            .await
            .expect("env present");
        assert_eq!(trimmed, 2);

        // s2 should still have 2 entries left; s1 should be empty.
        let s1_left = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry
                    .buffered_outbound_after_for_stream(None, "c1", "s1")
                    .len()
            })
            .await
            .expect("env present");
        let s2_left = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry
                    .buffered_outbound_after_for_stream(None, "c1", "s2")
                    .len()
            })
            .await
            .expect("env present");
        assert_eq!(s1_left, 0);
        assert_eq!(s2_left, 2);
    }

    #[tokio::test]
    async fn ack_with_segment_id_trims_segments_only_up_to_threshold() {
        let state = RelayState::new();
        let enrolled = state
            .enroll(key("acc", "install", "host"), "host".to_string())
            .await;
        state
            .with_env_mut(&enrolled.server_id, |env_entry| {
                for segment in 0u64..=3 {
                    let cursor = env_entry.allocate_cursor();
                    env_entry.push_outbound(entry(
                        parse_cursor(&cursor).unwrap_or(0),
                        "c1",
                        "s1",
                        Some(1),
                        Some(segment as usize),
                        "chunk",
                    ));
                }
            })
            .await;

        // Ack (seq=1, segment=1) → trim segments 0, 1 (cursor (1, 0), (1, 1)).
        let trimmed = state
            .with_env_mut(&enrolled.server_id, |env_entry| {
                env_entry.ack_outbound("c1", "s1", 1, Some(1))
            })
            .await
            .expect("env present");
        assert_eq!(trimmed, 2);

        let left = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry
                    .buffered_outbound_after_for_stream(None, "c1", "s1")
                    .len()
            })
            .await
            .expect("env present");
        assert_eq!(left, 2);
    }

    #[tokio::test]
    async fn outbound_replay_filters_by_stream() {
        let state = RelayState::new();
        let enrolled = state
            .enroll(key("acc", "install", "host"), "host".to_string())
            .await;
        state
            .with_env_mut(&enrolled.server_id, |env_entry| {
                for &(client, stream) in &[("c1", "s1"), ("c1", "s2"), ("c2", "s1")] {
                    let cursor = env_entry.allocate_cursor();
                    env_entry.push_outbound(entry(
                        parse_cursor(&cursor).unwrap_or(0),
                        client,
                        stream,
                        Some(1),
                        None,
                        "x",
                    ));
                }
            })
            .await;
        let c1_s1 = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry
                    .buffered_outbound_after_for_stream(None, "c1", "s1")
                    .len()
            })
            .await
            .expect("env present");
        let c1_s2 = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry
                    .buffered_outbound_after_for_stream(None, "c1", "s2")
                    .len()
            })
            .await
            .expect("env present");
        let c2_s1 = state
            .with_env(&enrolled.server_id, |env_entry| {
                env_entry
                    .buffered_outbound_after_for_stream(None, "c2", "s1")
                    .len()
            })
            .await
            .expect("env present");
        assert_eq!(c1_s1, 1);
        assert_eq!(c1_s2, 1);
        assert_eq!(c2_s1, 1);
    }

    #[tokio::test]
    async fn close_code_slot_is_set_once() {
        let slot = CloseCodeSlot::new();
        slot.set(1011, "backpressure");
        slot.set(4000, "should-be-ignored");
        let taken = slot.take().expect("slot was set");
        assert_eq!(taken.0, 1011);
        assert_eq!(taken.1, "backpressure");
        assert!(slot.take().is_none());
    }
}
