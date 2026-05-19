//! ChatGPT relay simulator for the `wham/remote/control/*` surface.
//!
//! Mirrors the wire protocol that the real ChatGPT backend exposes to Codex's
//! `app-server-transport::remote_control` and to the ChatGPT mobile app.
//! Codex (the local "server") connects out over WSS to
//! `GET /wham/remote/control/server`; a phone connects to the symmetric
//! `GET /wham/remote/control/client` endpoint. The relay buffers envelopes in
//! both directions so either side can disconnect, reconnect with a cursor, and
//! resume without loss.
//!
//! Wire types, constants, and helpers live in `protocol`. Long-lived shared
//! state (environments, replay buffers, link registry) lives in `state`.
//! Handlers are split per role: `enroll` for the HTTPS bootstrap step,
//! `codex_session` for the Codex WebSocket pump, and `phone_session` for the
//! mobile client WebSocket pump.
//!
//! This crate intentionally re-implements wire types instead of importing
//! `codex-app-server-transport`: the transport crate exposes its protocol
//! types as `pub(crate)`/`pub(super)`, and we keep the dependency direction
//! one-way so test/mock binaries never pull the live transport in.

pub(crate) mod codex_session;
pub(crate) mod enroll;
pub(crate) mod phone_session;
pub(crate) mod protocol;
pub(crate) mod state;

use std::convert::Infallible;

use warp::Filter;
use warp::filters::BoxedFilter;
use warp::reply::Response;

use crate::server::state::AppState;

pub(crate) type RemoteControlRoute = BoxedFilter<(Response,)>;

/// Combined filter mounting enroll + Codex WSS + Phone WSS handlers under
/// `/backend-api/wham/remote/control/*`.
pub(crate) fn routes(state: AppState) -> RemoteControlRoute {
    enroll::route(state.clone())
        .or(codex_session::route(state.clone()))
        .unify()
        .or(phone_session::route(state))
        .unify()
        .boxed()
}

pub(crate) fn with_state(
    state: AppState,
) -> impl Filter<Extract = (AppState,), Error = Infallible> + Clone {
    warp::any().map(move || state.clone())
}
