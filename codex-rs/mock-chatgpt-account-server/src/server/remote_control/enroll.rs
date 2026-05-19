//! `POST /backend-api/wham/remote/control/server/enroll` handler.
//!
//! Codex calls this once at startup (and on retry after errors) to register
//! its local installation with the relay. The relay returns a stable
//! `(server_id, environment_id)` pair keyed by
//! `(account_id, installation_id, name)`; repeat enrolls with the same key
//! return the same pair so reconnects survive process restarts.

use bytes::Bytes;
use serde_json::json;
use warp::Filter;
use warp::http::HeaderMap;
use warp::http::Method;
use warp::http::StatusCode;
use warp::http::header::CONTENT_LENGTH;
use warp::http::header::CONTENT_TYPE;
use warp::http::header::HeaderName;
use warp::http::header::HeaderValue;
use warp::reply::Response;

use crate::server::remote_control::RemoteControlRoute;
use crate::server::remote_control::protocol::EnrollRemoteServerRequest;
use crate::server::remote_control::protocol::EnrollRemoteServerResponse;
use crate::server::remote_control::protocol::REMOTE_CONTROL_ACCOUNT_ID_HEADER;
use crate::server::remote_control::protocol::REMOTE_CONTROL_INSTALLATION_ID_HEADER;
use crate::server::remote_control::state::EnrollKey;
use crate::server::remote_control::with_state;
use crate::server::state::AppState;

pub(crate) fn route(state: AppState) -> RemoteControlRoute {
    warp::path!("backend-api" / "wham" / "remote" / "control" / "server" / "enroll")
        .and(warp::method())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and(with_state(state))
        .and_then(handle)
        .boxed()
}

async fn handle(
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    state: AppState,
) -> Result<Response, std::convert::Infallible> {
    if method != Method::POST {
        return Ok(json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            &json!({ "error": "expected POST" }),
        ));
    }

    let Some(account_id) = chatgpt_account_id(&state, &headers) else {
        return Ok(unauthorized());
    };

    let Some(installation_id) = installation_id(&headers) else {
        return Ok(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": format!("missing {REMOTE_CONTROL_INSTALLATION_ID_HEADER} header") }),
        ));
    };

    let request: EnrollRemoteServerRequest = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                &json!({ "error": format!("invalid enroll body: {err}") }),
            ));
        }
    };
    if request.installation_id != installation_id {
        return Ok(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "installation_id mismatch between header and body" }),
        ));
    }

    let key = EnrollKey {
        account_id,
        installation_id,
        name: request.name.clone(),
    };
    let enrolled = state.relay().enroll(key, request.name.clone()).await;
    let payload = EnrollRemoteServerResponse {
        server_id: enrolled.server_id,
        environment_id: enrolled.environment_id,
    };
    Ok(json_response(StatusCode::OK, &payload))
}

fn chatgpt_account_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if !has_bearer(headers) {
        return None;
    }
    let account = header_value(headers, REMOTE_CONTROL_ACCOUNT_ID_HEADER)?;
    if state.args.strict_account_header && account != state.args.chatgpt_account_id {
        return None;
    }
    Some(account)
}

pub(crate) fn has_bearer(headers: &HeaderMap) -> bool {
    header_value(headers, "authorization")
        .map(|value| value.starts_with("Bearer "))
        .unwrap_or(false)
}

/// Browser-driven WebSocket clients (e.g. the in-tree React console) cannot
/// attach `Authorization` headers to the handshake; the relay therefore lets
/// WSS handlers accept the bearer token via the `bearer` query parameter as
/// a dev fallback. Returns true if either the header *or* the query param
/// supplies a non-empty bearer.
pub(crate) fn has_bearer_with_query_fallback(
    headers: &HeaderMap,
    query: &[(String, String)],
) -> bool {
    if has_bearer(headers) {
        return true;
    }
    query
        .iter()
        .find(|(k, _)| k == "bearer")
        .map(|(_, v)| !v.is_empty())
        .unwrap_or(false)
}

/// Symmetric fallback for any header the WSS handler reads: prefer the HTTP
/// header, fall back to a query parameter of the same name.
pub(crate) fn header_or_query(
    headers: &HeaderMap,
    query: &[(String, String)],
    name: &str,
) -> Option<String> {
    if let Some(value) = header_value(headers, name) {
        return Some(value);
    }
    query
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

fn installation_id(headers: &HeaderMap) -> Option<String> {
    header_value(headers, REMOTE_CONTROL_INSTALLATION_ID_HEADER).filter(|value| !value.is_empty())
}

pub(crate) fn unauthorized() -> Response {
    json_response(
        StatusCode::UNAUTHORIZED,
        &json!({ "error": "missing or invalid bearer token / chatgpt-account-id" }),
    )
}

pub(crate) fn json_response<T: serde::Serialize>(status: StatusCode, payload: &T) -> Response {
    let body = match serde_json::to_vec(payload) {
        Ok(body) => body,
        Err(_) => br#"{"error":"failed to serialize json response"}"#.to_vec(),
    };
    let length = body.len().to_string();
    let mut response = Response::new(warp::hyper::Body::from(body));
    *response.status_mut() = status;
    insert_header(&mut response, CONTENT_TYPE.as_str(), "application/json");
    insert_header(&mut response, CONTENT_LENGTH.as_str(), &length);
    response
}

fn insert_header(response: &mut Response, name: &str, value: &str) {
    let Ok(name) = HeaderName::try_from(name) else {
        return;
    };
    let Ok(value) = HeaderValue::from_str(value) else {
        return;
    };
    response.headers_mut().insert(name, value);
}
