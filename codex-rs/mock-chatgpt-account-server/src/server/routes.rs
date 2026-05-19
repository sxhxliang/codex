use std::collections::HashMap;
use std::convert::Infallible;

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use url::form_urlencoded;
use warp::Filter;
use warp::Reply;
use warp::filters::BoxedFilter;
use warp::http::HeaderMap;
use warp::http::HeaderValue;
use warp::http::Method;
use warp::http::StatusCode;
use warp::http::header::CACHE_CONTROL;
use warp::http::header::CONTENT_LENGTH;
use warp::http::header::CONTENT_TYPE;
use warp::http::header::HeaderName;
use warp::http::header::LOCATION;
use warp::http::header::SET_COOKIE;
use warp::path::FullPath;
use warp::reply::Response;
use warp::sse::Event;
use warp::ws::Message;
use warp::ws::WebSocket;
use warp::ws::Ws;

use crate::server::frontend::PageBootstrap;
use crate::server::frontend::StaticAsset;
use crate::server::frontend::asset_bytes;
use crate::server::frontend::render_page;
use crate::server::responses::build_responses_events;
use crate::server::responses::extract_text_fragments;
use crate::server::responses_proxy::maybe_proxy_responses_request;
use crate::server::state::AppState;
use crate::server::state::BROWSER_SESSION_COOKIE;
use crate::server::state::CALENDAR_CREATE_EVENT_RESOURCE_URI;
use crate::server::state::CALENDAR_LIST_EVENTS_RESOURCE_URI;
use crate::server::state::CONNECTOR_DESCRIPTION;
use crate::server::state::CONNECTOR_ID;
use crate::server::state::CONNECTOR_NAME;
use crate::server::state::DISCOVERABLE_CALENDAR_ID;
use crate::server::state::DISCOVERABLE_GMAIL_ID;
use crate::server::state::DeviceTokenStatus;
use crate::server::state::MCP_PROTOCOL_VERSION;
use crate::server::state::MCP_SERVER_NAME;
use crate::server::state::MCP_SERVER_VERSION;
use crate::server::state::RequirementsResponse;
use crate::server::state::unix_now;

type HttpRoute = BoxedFilter<(Response,)>;

pub fn routes(state: AppState) -> HttpRoute {
    super::remote_control::routes(state.clone())
        .or(websocket_routes(state.clone()))
        .unify()
        .or(http_routes(state))
        .unify()
        .boxed()
}

fn websocket_routes(state: AppState) -> HttpRoute {
    let backend_ws = warp::path!("backend-api" / "codex" / "responses")
        .and(warp::get())
        .and(warp::ws())
        .and(warp::header::headers_cloned())
        .and(with_state(state.clone()))
        .and_then(handle_websocket_upgrade);

    let v1_ws = warp::path!("v1" / "responses")
        .and(warp::get())
        .and(warp::ws())
        .and(warp::header::headers_cloned())
        .and(with_state(state))
        .and_then(handle_websocket_upgrade);

    backend_ws.or(v1_ws).unify().boxed()
}

fn http_routes(state: AppState) -> HttpRoute {
    warp::any()
        .and(warp::method())
        .and(warp::path::full())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and(with_state(state))
        .and_then(dispatch_http)
        .boxed()
}

fn with_state(state: AppState) -> impl Filter<Extract = (AppState,), Error = Infallible> + Clone {
    warp::any().map(move || state.clone())
}

async fn dispatch_http(
    method: Method,
    full_path: FullPath,
    query: String,
    headers: HeaderMap,
    body: Bytes,
    state: AppState,
) -> Result<Response, Infallible> {
    let path = full_path.as_str();
    let target = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    let response = match (method.clone(), path) {
        (Method::GET, "/healthz") => json_response(StatusCode::OK, &json!({ "ok": true })),
        (Method::GET, "/assets/app.css") => static_asset_response(StaticAsset::AppCss),
        (Method::GET, "/assets/app.js") => static_asset_response(StaticAsset::AppJs),
        (Method::GET, "/models")
        | (Method::GET, "/v1/models")
        | (Method::GET, "/backend-api/codex/models") => handle_models(&state),
        (Method::GET, "/.well-known/oauth-authorization-server/mcp") => {
            handle_mcp_oauth_metadata(&state, &headers)
        }
        (Method::GET, "/connectors/directory/list")
        | (Method::GET, "/backend-api/connectors/directory/list") => handle_connectors_directory(),
        (Method::GET, "/connectors/directory/list_workspace")
        | (Method::GET, "/backend-api/connectors/directory/list_workspace") => {
            handle_connectors_directory_workspace()
        }
        (Method::GET, "/oauth/authorize") => handle_authorize(&state, &query, &headers).await,
        (Method::GET, "/oauth/logout") => handle_browser_logout(&state, &query, &headers).await,
        (Method::GET, "/codex/device") => handle_device_page(&state, &headers, None).await,
        (Method::GET, "/codex/remote-control") => handle_remote_control_page(&state, &headers),
        (Method::GET, "/deviceauth/callback") => handle_callback_page(&headers),
        (Method::POST, "/oauth/login") => handle_browser_login(&state, &headers, &body).await,
        (Method::POST, "/oauth/login/shortcut") => {
            handle_browser_shortcut_login(&state, &headers, &body).await
        }
        (Method::POST, "/oauth/authorize/approve") => {
            handle_authorize_approve(&state, &headers, &body).await
        }
        (Method::POST, "/oauth/token") => handle_oauth_token(&state, &headers, &body),
        (Method::POST, "/backend-api/codex/responses") | (Method::POST, "/v1/responses") => {
            handle_responses(&state, &headers, &body).await
        }
        (Method::POST, "/api/accounts/deviceauth/usercode") => handle_device_usercode(&state).await,
        (Method::POST, "/api/accounts/deviceauth/token") => {
            handle_device_token(&state, &body).await
        }
        (Method::POST, "/codex/device") => handle_device_approval(&state, &headers, &body).await,
        _ => {
            if method == Method::GET {
                if let Some(provider_id) = parse_social_login_callback_path(path) {
                    handle_browser_shortcut_callback(&state, &headers, &query, provider_id).await
                } else if path == "/api/codex/usage"
                    || path == "/wham/usage"
                    || path == "/backend-api/wham/usage"
                {
                    handle_usage(&state, &headers)
                } else if path == "/api/codex/config/requirements"
                    || path == "/wham/config/requirements"
                    || path == "/backend-api/wham/config/requirements"
                {
                    handle_config_requirements(&state, &headers)
                } else if path == "/api/codex/tasks/list"
                    || path == "/wham/tasks/list"
                    || path == "/backend-api/wham/tasks/list"
                {
                    handle_task_list(&state, &headers).await
                } else if path == "/api/codex/tasks"
                    || path == "/wham/tasks"
                    || path == "/backend-api/wham/tasks"
                {
                    handle_task_create(&state, &headers, &body).await
                } else if path == "/api/codex/apps"
                    || path == "/wham/apps"
                    || path == "/backend-api/wham/apps"
                {
                    handle_apps_json_rpc(&state, &headers, &body)
                } else if let Some(task_id) = path.strip_prefix("/codex/tasks/") {
                    handle_task_page(&state, &headers, task_id).await
                } else if let Some(task_path) = parse_task_details_path(path) {
                    handle_task_details_path(&state, &headers, task_path).await
                } else {
                    json_response(
                        StatusCode::NOT_FOUND,
                        &json!({ "error": format!("unknown path: {path}") }),
                    )
                }
            } else if path == "/api/codex/usage"
                || path == "/wham/usage"
                || path == "/backend-api/wham/usage"
            {
                handle_usage(&state, &headers)
            } else if path == "/api/codex/config/requirements"
                || path == "/wham/config/requirements"
                || path == "/backend-api/wham/config/requirements"
            {
                handle_config_requirements(&state, &headers)
            } else if path == "/api/codex/tasks/list"
                || path == "/wham/tasks/list"
                || path == "/backend-api/wham/tasks/list"
            {
                handle_task_list(&state, &headers).await
            } else if path == "/api/codex/tasks"
                || path == "/wham/tasks"
                || path == "/backend-api/wham/tasks"
            {
                handle_task_create(&state, &headers, &body).await
            } else if path == "/api/codex/apps"
                || path == "/wham/apps"
                || path == "/backend-api/wham/apps"
            {
                handle_apps_json_rpc(&state, &headers, &body)
            } else if let Some(task_id) = path.strip_prefix("/codex/tasks/") {
                handle_task_page(&state, &headers, task_id).await
            } else if let Some(task_path) = parse_task_details_path(path) {
                handle_task_details_path(&state, &headers, task_path).await
            } else {
                json_response(
                    StatusCode::NOT_FOUND,
                    &json!({ "error": format!("unknown path: {path}") }),
                )
            }
        }
    };
    println!(
        "[mock-account-server] {} {} -> {}",
        method,
        target,
        response.status()
    );
    Ok(response)
}

async fn handle_websocket_upgrade(
    ws: Ws,
    headers: HeaderMap,
    state: AppState,
) -> Result<Response, Infallible> {
    if let Err(response) = require_chatgpt_auth(&state, &headers) {
        return Ok(response);
    }

    Ok(ws
        .on_upgrade(move |socket| async move {
            handle_responses_socket(socket, state).await;
        })
        .into_response())
}

async fn handle_responses_socket(mut socket: WebSocket, state: AppState) {
    while let Some(message) = socket.next().await {
        let Ok(message) = message else {
            break;
        };

        if message.is_close() {
            break;
        }
        if message.is_ping() {
            let _ = socket
                .send(Message::pong(message.as_bytes().to_vec()))
                .await;
            continue;
        }
        if !message.is_text() {
            let _ = socket
                .send(Message::text(websocket_error_json(
                    "unsupported websocket opcode",
                )))
                .await;
            continue;
        }

        let request_payload = match serde_json::from_slice::<Value>(message.as_bytes()) {
            Ok(payload) => payload,
            Err(_) => {
                let _ = socket
                    .send(Message::text(websocket_error_json(
                        "invalid websocket JSON payload",
                    )))
                    .await;
                continue;
            }
        };

        if request_payload.get("type").and_then(Value::as_str) != Some("response.create") {
            let _ = socket
                .send(Message::text(websocket_error_json(
                    "expected websocket request type response.create",
                )))
                .await;
            continue;
        }

        for event in build_responses_events(&state, &request_payload) {
            let Ok(payload) = serde_json::to_string(&event) else {
                continue;
            };
            if socket.send(Message::text(payload)).await.is_err() {
                break;
            }
        }
    }
}

fn handle_models(state: &AppState) -> Response {
    let mut response = json_response(StatusCode::OK, &state.build_models_response());
    set_header_str(&mut response, "etag", &state.args.models_etag);
    response
}

fn handle_mcp_oauth_metadata(state: &AppState, headers: &HeaderMap) -> Response {
    let origin = request_origin(state, headers);
    json_response(
        StatusCode::OK,
        &json!({
            "authorization_endpoint": format!("{origin}/oauth/authorize"),
            "token_endpoint": format!("{origin}/oauth/token"),
            "scopes_supported": [""],
        }),
    )
}

fn handle_connectors_directory() -> Response {
    json_response(
        StatusCode::OK,
        &json!({
            "apps": [
                {
                    "id": DISCOVERABLE_CALENDAR_ID,
                    "name": "Google Calendar",
                    "description": "Plan events and schedules.",
                },
                {
                    "id": DISCOVERABLE_GMAIL_ID,
                    "name": "Gmail",
                    "description": "Find and summarize email threads.",
                }
            ],
            "nextToken": Value::Null,
        }),
    )
}

fn handle_connectors_directory_workspace() -> Response {
    json_response(
        StatusCode::OK,
        &json!({
            "apps": [],
            "nextToken": Value::Null,
        }),
    )
}

async fn handle_authorize(state: &AppState, query: &str, headers: &HeaderMap) -> Response {
    let params = parse_urlencoded(query);
    let Some(redirect_uri) = params.get("redirect_uri") else {
        println!("[mock-account-server] /oauth/authorize rejected: missing redirect_uri");
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "redirect_uri is required" }),
        );
    };
    let state_value = params.get("state").cloned().unwrap_or_default();
    let browser_session = cookie_value(headers, BROWSER_SESSION_COOKIE);
    let Some(session_username) = state
        .browser_session_username(browser_session.as_deref())
        .await
    else {
        println!(
            "[mock-account-server] /oauth/authorize requires login before redirecting to {redirect_uri}"
        );
        let continue_to = if query.is_empty() {
            "/oauth/authorize".to_string()
        } else {
            format!("/oauth/authorize?{query}")
        };
        return browser_login_page(StatusCode::OK, state, headers, continue_to, None);
    };
    println!(
        "[mock-account-server] /oauth/authorize confirmed browser session for {session_username}, redirect_uri={redirect_uri}"
    );

    let continue_to = if query.is_empty() {
        "/oauth/authorize".to_string()
    } else {
        format!("/oauth/authorize?{query}")
    };
    respond_page(
        StatusCode::OK,
        headers,
        &PageBootstrap::AccountConfirm {
            continue_to,
            email: session_username,
            account_id: state.args.chatgpt_account_id.clone(),
            plan_type: state.args.plan_type.clone(),
            organization_id: state
                .args
                .organization_id
                .clone()
                .unwrap_or_else(|| state.args.chatgpt_account_id.clone()),
            project_id: state.args.project_id.clone(),
            redirect_uri: redirect_uri.clone(),
            oauth_state: state_value,
        },
    )
}

async fn handle_authorize_approve(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    let params = parse_urlencoded_body(body);
    let continue_to = normalize_continue_to(
        params
            .get("continue_to")
            .cloned()
            .unwrap_or_else(|| "/oauth/authorize".to_string()),
    );
    let browser_session = cookie_value(headers, BROWSER_SESSION_COOKIE);
    if !state.has_browser_session(browser_session.as_deref()).await {
        println!(
            "[mock-account-server] /oauth/authorize/approve rejected: expired browser session"
        );
        return browser_login_page(
            StatusCode::UNAUTHORIZED,
            state,
            headers,
            continue_to,
            Some("Your browser session expired. Please sign in again.".to_string()),
        );
    }

    let authorize_query = continue_to
        .strip_prefix("/oauth/authorize")
        .and_then(|value| value.strip_prefix('?'))
        .unwrap_or_default();
    let authorize_params = parse_urlencoded(authorize_query);
    let Some(redirect_uri) = authorize_params.get("redirect_uri") else {
        println!(
            "[mock-account-server] /oauth/authorize/approve rejected: continue_to missing redirect_uri"
        );
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "redirect_uri is required" }),
        );
    };
    let state_value = authorize_params.get("state").cloned().unwrap_or_default();

    let code = state.create_auth_code("auth").await;
    let separator = if redirect_uri.contains('?') { "&" } else { "?" };
    let callback_url = format!("{redirect_uri}{separator}code={code}&state={state_value}");
    println!(
        "[mock-account-server] /oauth/authorize/approve issued auth code for redirect_uri={redirect_uri}"
    );
    if accepts_json(headers) {
        json_response(
            StatusCode::OK,
            &json!({
                "ok": true,
                "redirectTo": callback_url,
            }),
        )
    } else {
        redirect_response(&callback_url, None)
    }
}

async fn handle_browser_login(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    let params = parse_urlencoded_body(body);
    let username = params.get("username").cloned().unwrap_or_default();
    let password = params.get("password").cloned().unwrap_or_default();
    let continue_to = normalize_continue_to(
        params
            .get("continue_to")
            .cloned()
            .unwrap_or_else(|| "/oauth/authorize".to_string()),
    );
    println!(
        "[mock-account-server] /oauth/login attempt username={username}, continue_to={continue_to}"
    );

    let expected_username = state
        .args
        .login_username
        .clone()
        .unwrap_or_else(|| state.args.email.clone());
    if username != expected_username || password != state.args.login_password {
        println!("[mock-account-server] /oauth/login failed for username={username}");
        if accepts_json(headers) {
            return json_response(
                StatusCode::UNAUTHORIZED,
                &json!({
                    "ok": false,
                    "errorMessage": "Invalid username or password.",
                }),
            );
        }
        return browser_login_page(
            StatusCode::OK,
            state,
            headers,
            continue_to,
            Some("Invalid username or password.".to_string()),
        );
    }

    let session_id = state.create_browser_session(&username).await;
    let cookie = format!("{BROWSER_SESSION_COOKIE}={session_id}; HttpOnly; Path=/; SameSite=Lax");
    println!("[mock-account-server] /oauth/login succeeded for username={username}");
    if accepts_json(headers) {
        let mut response = json_response(
            StatusCode::OK,
            &json!({
                "ok": true,
                "redirectTo": continue_to,
            }),
        );
        set_header_str(&mut response, SET_COOKIE.as_str(), &cookie);
        return response;
    }
    redirect_response(&continue_to, Some(cookie))
}

async fn handle_browser_shortcut_login(
    state: &AppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let params = parse_urlencoded_body(body);
    let provider_id = params.get("provider").cloned().unwrap_or_default();
    let continue_to = normalize_continue_to(
        params
            .get("continue_to")
            .cloned()
            .unwrap_or_else(|| "/oauth/authorize".to_string()),
    );
    println!(
        "[mock-account-server] /oauth/login/shortcut provider={provider_id}, continue_to={continue_to}"
    );
    let Some(provider) = state.social_login_provider(&provider_id) else {
        println!(
            "[mock-account-server] /oauth/login/shortcut rejected: provider={provider_id} not configured"
        );
        if accepts_json(headers) {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({
                    "ok": false,
                    "errorMessage": "Shortcut login provider is not configured.",
                }),
            );
        }
        return browser_login_page(
            StatusCode::OK,
            state,
            headers,
            continue_to,
            Some("Shortcut login provider is not configured.".to_string()),
        );
    };
    let provider = provider.clone();
    let oauth_state = state
        .create_social_login_state(&provider_id, &continue_to)
        .await;
    let authorize_url = match provider.authorize_url(
        &social_login_callback_url(state, headers, &provider_id),
        &oauth_state,
    ) {
        Ok(authorize_url) => authorize_url,
        Err(error) => {
            println!(
                "[mock-account-server] /oauth/login/shortcut failed for provider={provider_id}: {error}"
            );
            if accepts_json(headers) {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({
                        "ok": false,
                        "errorMessage": error.to_string(),
                    }),
                );
            }
            return browser_login_page(
                StatusCode::OK,
                state,
                headers,
                continue_to,
                Some(error.to_string()),
            );
        }
    };
    println!(
        "[mock-account-server] /oauth/login/shortcut redirecting provider={provider_id} to upstream authorize URL"
    );
    if accepts_json(headers) {
        return json_response(
            StatusCode::OK,
            &json!({
                "ok": true,
                "redirectTo": authorize_url,
            }),
        );
    }
    redirect_response(&authorize_url, None)
}

async fn handle_browser_shortcut_callback(
    state: &AppState,
    headers: &HeaderMap,
    query: &str,
    provider_id: &str,
) -> Response {
    let default_continue_to = "/oauth/authorize".to_string();
    println!("[mock-account-server] /oauth/login/{provider_id}/callback query={query}");
    let Some(provider) = state.social_login_provider(provider_id).cloned() else {
        println!(
            "[mock-account-server] /oauth/login/{provider_id}/callback rejected: provider not configured"
        );
        return browser_login_page(
            StatusCode::BAD_REQUEST,
            state,
            headers,
            default_continue_to,
            Some("Shortcut login provider is not configured.".to_string()),
        );
    };
    let params = parse_urlencoded(query);
    let Some(oauth_state) = params.get("state").cloned() else {
        println!(
            "[mock-account-server] /oauth/login/{provider_id}/callback rejected: missing state"
        );
        return browser_login_page(
            StatusCode::BAD_REQUEST,
            state,
            headers,
            default_continue_to,
            Some(format!(
                "{} login callback is missing the state parameter.",
                provider.label
            )),
        );
    };
    let Some(continue_to) = state
        .consume_social_login_continue_to(provider_id, &oauth_state)
        .await
    else {
        println!(
            "[mock-account-server] /oauth/login/{provider_id}/callback rejected: invalid or expired state"
        );
        return browser_login_page(
            StatusCode::BAD_REQUEST,
            state,
            headers,
            default_continue_to,
            Some(format!(
                "{} login state expired or is invalid.",
                provider.label
            )),
        );
    };
    if let Some(error_code) = params.get("error") {
        let error_message = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error_code.clone());
        println!(
            "[mock-account-server] /oauth/login/{provider_id}/callback upstream error: {error_message}"
        );
        return browser_login_page(
            StatusCode::BAD_REQUEST,
            state,
            headers,
            continue_to,
            Some(format!("{} login failed: {error_message}", provider.label)),
        );
    }
    let Some(code) = params.get("code").cloned() else {
        println!(
            "[mock-account-server] /oauth/login/{provider_id}/callback rejected: missing authorization code"
        );
        return browser_login_page(
            StatusCode::BAD_REQUEST,
            state,
            headers,
            continue_to,
            Some(format!(
                "{} login callback is missing the authorization code.",
                provider.label
            )),
        );
    };

    match provider
        .exchange_code_for_identity(
            &code,
            &social_login_callback_url(state, headers, provider_id),
        )
        .await
    {
        Ok(identity) => {
            println!(
                "[mock-account-server] /oauth/login/{provider_id}/callback resolved identity={}",
                identity.session_username
            );
            let session_id = state
                .create_browser_session(&identity.session_username)
                .await;
            let cookie =
                format!("{BROWSER_SESSION_COOKIE}={session_id}; HttpOnly; Path=/; SameSite=Lax");
            redirect_response(&continue_to, Some(cookie))
        }
        Err(error) => {
            println!(
                "[mock-account-server] /oauth/login/{provider_id}/callback exchange failed: {error}"
            );
            browser_login_page(
                StatusCode::BAD_GATEWAY,
                state,
                headers,
                continue_to,
                Some(format!("{} login failed: {error}", provider.label)),
            )
        }
    }
}

async fn handle_browser_logout(state: &AppState, query: &str, headers: &HeaderMap) -> Response {
    let params = parse_urlencoded(query);
    let continue_to = normalize_logout_continue_to(
        params
            .get("continue_to")
            .cloned()
            .unwrap_or_else(|| "/oauth/authorize".to_string()),
    );
    let browser_session = cookie_value(headers, BROWSER_SESSION_COOKIE);
    state
        .clear_browser_session(browser_session.as_deref())
        .await;
    println!("[mock-account-server] /oauth/logout cleared browser session");

    redirect_response(
        &continue_to,
        Some(format!(
            "{BROWSER_SESSION_COOKIE}=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; Path=/; SameSite=Lax"
        )),
    )
}

fn handle_oauth_token(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    let content_type = header_value(headers, CONTENT_TYPE.as_str()).unwrap_or_default();
    println!("[mock-account-server] /oauth/token content_type={content_type}");
    let params = if content_type.contains("application/x-www-form-urlencoded") {
        parse_urlencoded_body(body)
    } else if content_type.contains("application/json") {
        let payload = match parse_json_object(body) {
            Ok(payload) => payload,
            Err(response) => return response,
        };
        payload
            .as_object()
            .into_iter()
            .flat_map(|object| object.iter())
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
            .collect()
    } else {
        println!(
            "[mock-account-server] /oauth/token rejected unsupported content_type={content_type}"
        );
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "expected application/json or application/x-www-form-urlencoded" }),
        );
    };
    let grant_type = params.get("grant_type").cloned().unwrap_or_default();
    println!("[mock-account-server] /oauth/token grant_type={grant_type}");
    if grant_type == "authorization_code" {
        println!("[mock-account-server] /oauth/token issuing authorization_code tokens");
        return json_response(
            StatusCode::OK,
            &json!({
                "id_token": state.build_id_token(),
                "access_token": state.build_access_token(),
                "refresh_token": state.args.refresh_token,
            }),
        );
    }
    if grant_type == "refresh_token" {
        let refresh_token = params.get("refresh_token").cloned().unwrap_or_default();
        if refresh_token != state.args.refresh_token {
            println!("[mock-account-server] /oauth/token rejected refresh_token: token mismatch");
            return json_response(
                StatusCode::UNAUTHORIZED,
                &json!({
                    "error": {
                        "code": "refresh_token_invalidated",
                        "message": "refresh token is not recognized by the mock server",
                    }
                }),
            );
        }
        println!("[mock-account-server] /oauth/token refresh_token accepted");
        return json_response(
            StatusCode::OK,
            &json!({
                "id_token": state.build_id_token(),
                "access_token": state.build_access_token(),
                "refresh_token": state.args.refresh_token,
            }),
        );
    }
    if grant_type == "urn:ietf:params:oauth:grant-type:token-exchange" {
        println!("[mock-account-server] /oauth/token issuing token-exchange api key");
        return json_response(
            StatusCode::OK,
            &json!({ "access_token": state.args.api_key }),
        );
    }
    println!("[mock-account-server] /oauth/token rejected unsupported grant_type={grant_type}");

    json_response(
        StatusCode::BAD_REQUEST,
        &json!({ "error": format!("unsupported grant_type: {grant_type}") }),
    )
}

async fn handle_responses(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }
    if let Some(response) = maybe_proxy_responses_request(state, headers, body).await {
        return response;
    }
    let payload = match parse_json_object(body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    sse_response(build_responses_events(state, &payload))
}

async fn handle_device_usercode(state: &AppState) -> Response {
    let record = state.create_device_code().await;
    json_response(
        StatusCode::OK,
        &json!({
            "device_auth_id": record.device_auth_id,
            "user_code": record.user_code,
            "interval": state.args.device_code_interval_secs.to_string(),
        }),
    )
}

async fn handle_device_token(state: &AppState, body: &Bytes) -> Response {
    let payload = match parse_json_object(body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let device_auth_id = payload
        .get("device_auth_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let user_code = payload
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match state.poll_device_token(device_auth_id, user_code).await {
        DeviceTokenStatus::Unknown => json_response(
            StatusCode::NOT_FOUND,
            &json!({ "error": "unknown device code" }),
        ),
        DeviceTokenStatus::Pending => {
            json_response(StatusCode::NOT_FOUND, &json!({ "status": "pending" }))
        }
        DeviceTokenStatus::Approved {
            authorization_code,
            code_challenge,
            code_verifier,
        } => json_response(
            StatusCode::OK,
            &json!({
                "authorization_code": authorization_code,
                "code_challenge": code_challenge,
                "code_verifier": code_verifier,
            }),
        ),
    }
}

async fn handle_device_page(
    state: &AppState,
    headers: &HeaderMap,
    message: Option<String>,
) -> Response {
    let bootstrap = PageBootstrap::DeviceAuth {
        records: state.device_code_views().await,
        message,
    };
    respond_page(StatusCode::OK, headers, &bootstrap)
}

async fn handle_device_approval(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    let form = parse_urlencoded_body(body);
    let user_code = form.get("user_code").cloned().unwrap_or_default();
    let message = if state.mark_device_code_approved(&user_code).await {
        "Approved device code.".to_string()
    } else {
        "Device code not found.".to_string()
    };
    handle_device_page(state, headers, Some(message)).await
}

fn handle_callback_page(headers: &HeaderMap) -> Response {
    respond_page(StatusCode::OK, headers, &PageBootstrap::Callback)
}

fn handle_remote_control_page(state: &AppState, headers: &HeaderMap) -> Response {
    let origin = request_origin(state, headers);
    let backend_base_url = format!("{origin}/backend-api");
    let bootstrap = PageBootstrap::RemoteControl {
        backend_base_url,
        bearer_token: state.args.access_token.clone(),
        account_id: state.args.chatgpt_account_id.clone(),
        suggested_installation_id: format!("install-{}", uuid::Uuid::new_v4().simple()),
        suggested_server_name: "Mock host".to_string(),
        strict_account_header: state.args.strict_account_header,
        protocol_version: crate::server::remote_control::protocol::REMOTE_CONTROL_PROTOCOL_VERSION
            .to_string(),
    };
    respond_page(StatusCode::OK, headers, &bootstrap)
}

fn handle_usage(state: &AppState, headers: &HeaderMap) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }

    let now = unix_now();
    json_response(
        StatusCode::OK,
        &json!({
            "plan_type": state.args.plan_type,
            "rate_limit": {
                "allowed": true,
                "limit_reached": state.args.primary_used_percent >= 100,
                "primary_window": {
                    "used_percent": state.args.primary_used_percent,
                    "limit_window_seconds": state.args.primary_window_mins * 60,
                    "reset_after_seconds": state.args.primary_resets_in_secs,
                    "reset_at": now + state.args.primary_resets_in_secs,
                },
                "secondary_window": {
                    "used_percent": state.args.secondary_used_percent,
                    "limit_window_seconds": state.args.secondary_window_mins * 60,
                    "reset_after_seconds": state.args.secondary_resets_in_secs,
                    "reset_at": now + state.args.secondary_resets_in_secs,
                },
            },
            "additional_rate_limits": state
                .args
                .additional_limit
                .iter()
                .map(|bucket| bucket.as_usage_payload(now))
                .collect::<Vec<_>>(),
        }),
    )
}

fn handle_config_requirements(state: &AppState, headers: &HeaderMap) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }

    let requirements: RequirementsResponse = state.requirements_response();
    json_response(StatusCode::OK, &requirements)
}

async fn handle_task_list(state: &AppState, headers: &HeaderMap) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }

    let items = state
        .list_tasks()
        .await
        .into_iter()
        .map(|task| task.as_list_item())
        .collect::<Vec<_>>();
    json_response(
        StatusCode::OK,
        &json!({
            "items": items,
            "cursor": Value::Null,
        }),
    )
}

async fn handle_task_create(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }

    let payload = match parse_json_object(body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };

    let mut prompt = extract_text_fragments(Some(&payload)).join("\n");
    prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        for key in ["prompt", "title", "instructions"] {
            if let Some(text) = payload.get(key).and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                prompt = text.trim().to_string();
                break;
            }
        }
    }

    let task = state.create_task(&prompt).await;
    json_response(
        StatusCode::OK,
        &json!({
            "task": { "id": task.task_id },
        }),
    )
}

async fn handle_task_page(state: &AppState, headers: &HeaderMap, task_id: &str) -> Response {
    let task = state.get_task(task_id).await;
    let status = if task.is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let bootstrap = PageBootstrap::TaskView {
        task: task.map(|task| task.as_page_data()),
        missing_task_id: if status == StatusCode::NOT_FOUND {
            Some(task_id.to_string())
        } else {
            None
        },
    };
    respond_page(status, headers, &bootstrap)
}

async fn handle_task_details_path(
    state: &AppState,
    headers: &HeaderMap,
    path: TaskDetailsPath,
) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }

    match path {
        TaskDetailsPath::Details { task_id } => {
            let Some(task) = state.get_task(&task_id).await else {
                return json_response(
                    StatusCode::NOT_FOUND,
                    &json!({ "error": format!("unknown task: {task_id}") }),
                );
            };
            json_response(StatusCode::OK, &task.as_turn_details())
        }
        TaskDetailsPath::SiblingTurns { task_id, turn_id } => {
            let Some(task) = state.get_task(&task_id).await else {
                return json_response(
                    StatusCode::NOT_FOUND,
                    &json!({ "error": format!("unknown task: {task_id}") }),
                );
            };
            json_response(
                StatusCode::OK,
                &json!({
                    "sibling_turns": [
                        {
                            "id": turn_id,
                            "task_id": task.task_id,
                            "attempt_placement": 0,
                        }
                    ]
                }),
            )
        }
    }
}

fn handle_apps_json_rpc(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    if let Err(response) = require_chatgpt_auth(state, headers) {
        return response;
    }

    let payload = match parse_json_object(body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let request_id = payload.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = payload.get("method").and_then(Value::as_str) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "missing method in JSON-RPC request" }),
        );
    };

    if method == "initialize" {
        let protocol_version = payload
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION);
        return json_response(
            StatusCode::OK,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": {
                        "name": MCP_SERVER_NAME,
                        "version": MCP_SERVER_VERSION,
                    },
                },
            }),
        );
    }

    if method == "notifications/initialized" || method.starts_with("notifications/") {
        return empty_response(StatusCode::ACCEPTED);
    }

    if method == "tools/list" {
        return json_response(
            StatusCode::OK,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "tools": [
                        {
                            "name": "calendar_create_event",
                            "description": "Create a calendar event.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "title": { "type": "string" },
                                    "starts_at": { "type": "string" },
                                    "timezone": { "type": "string" },
                                },
                                "required": ["title", "starts_at"],
                                "additionalProperties": false,
                            },
                            "_meta": {
                                "connector_id": CONNECTOR_ID,
                                "connector_name": CONNECTOR_NAME,
                                "connector_description": CONNECTOR_DESCRIPTION,
                                "_codex_apps": {
                                    "resource_uri": CALENDAR_CREATE_EVENT_RESOURCE_URI,
                                    "contains_mcp_source": true,
                                    "connector_id": CONNECTOR_ID,
                                },
                            },
                        },
                        {
                            "name": "calendar_list_events",
                            "description": "List calendar events.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "query": { "type": "string" },
                                    "limit": { "type": "integer" },
                                },
                                "additionalProperties": false,
                            },
                            "_meta": {
                                "connector_id": CONNECTOR_ID,
                                "connector_name": CONNECTOR_NAME,
                                "connector_description": CONNECTOR_DESCRIPTION,
                                "_codex_apps": {
                                    "resource_uri": CALENDAR_LIST_EVENTS_RESOURCE_URI,
                                    "contains_mcp_source": true,
                                    "connector_id": CONNECTOR_ID,
                                },
                            },
                        }
                    ],
                    "nextCursor": Value::Null,
                },
            }),
        );
    }

    if method == "tools/call" {
        let params = payload
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let arguments = params
            .get("arguments")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let codex_apps_meta = params
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get("_codex_apps"))
            .cloned()
            .unwrap_or(Value::Null);
        let tool_name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title = arguments
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let starts_at = arguments
            .get("starts_at")
            .and_then(Value::as_str)
            .unwrap_or_default();

        return json_response(
            StatusCode::OK,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": format!("called {tool_name} for {title} at {starts_at}"),
                        }
                    ],
                    "structuredContent": {
                        "_codex_apps": codex_apps_meta,
                    },
                    "isError": false,
                },
            }),
        );
    }

    json_response(
        StatusCode::OK,
        &json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {
                "code": -32601,
                "message": format!("method not found: {method}"),
            },
        }),
    )
}

fn respond_page(status: StatusCode, headers: &HeaderMap, bootstrap: &PageBootstrap) -> Response {
    if accepts_json(headers) {
        json_response(status, bootstrap)
    } else {
        html_response(status, render_page(bootstrap))
    }
}

fn browser_login_page(
    status: StatusCode,
    state: &AppState,
    headers: &HeaderMap,
    continue_to: String,
    error_message: Option<String>,
) -> Response {
    respond_page(
        status,
        headers,
        &PageBootstrap::BrowserLogin {
            continue_to,
            username_hint: state
                .args
                .login_username
                .clone()
                .unwrap_or_else(|| state.args.email.clone()),
            password_hint: state.args.login_password.clone(),
            error_message,
            social_providers: state.social_login_providers(),
        },
    )
}

fn social_login_callback_url(state: &AppState, headers: &HeaderMap, provider_id: &str) -> String {
    let origin = request_origin(state, headers);
    format!("{origin}/oauth/login/{provider_id}/callback")
}

fn parse_json_object(body: &Bytes) -> Result<Value, Response> {
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "invalid json body" }),
        )
    })?;
    if !value.is_object() {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "expected top-level JSON object" }),
        ));
    }
    Ok(value)
}

fn require_chatgpt_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let auth_header = header_value(headers, "authorization");
    let account_id = header_value(headers, "chatgpt-account-id");
    if !auth_header
        .as_deref()
        .map(|value| value.starts_with("Bearer "))
        .unwrap_or(false)
    {
        return Err(json_response(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "missing bearer token" }),
        ));
    }
    if state.args.strict_account_header
        && account_id.as_deref() != Some(state.args.chatgpt_account_id.as_str())
    {
        return Err(json_response(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "chatgpt-account-id mismatch" }),
        ));
    }
    Ok(())
}

fn request_origin(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(host) = header_value(headers, "host") {
        format!("http://{host}")
    } else {
        format!("http://{}:{}", state.args.host, state.args.port)
    }
}

fn accepts_json(headers: &HeaderMap) -> bool {
    header_value(headers, "accept")
        .map(|accept| accept.contains("application/json"))
        .unwrap_or(false)
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie_header = header_value(headers, "cookie")?;
    for part in cookie_header.split(';') {
        let trimmed = part.trim();
        let (cookie_name, cookie_value) = trimmed.split_once('=')?;
        if cookie_name == name {
            return Some(cookie_value.to_string());
        }
    }
    None
}

fn parse_urlencoded(input: &str) -> HashMap<String, String> {
    form_urlencoded::parse(input.as_bytes())
        .into_owned()
        .collect()
}

fn parse_urlencoded_body(body: &Bytes) -> HashMap<String, String> {
    match std::str::from_utf8(body) {
        Ok(value) => parse_urlencoded(value),
        Err(_) => HashMap::new(),
    }
}

fn normalize_continue_to(value: String) -> String {
    if value.starts_with("/oauth/authorize") {
        value
    } else {
        "/oauth/authorize".to_string()
    }
}

fn normalize_logout_continue_to(value: String) -> String {
    if value.starts_with('/') {
        value
    } else {
        "/oauth/authorize".to_string()
    }
}

fn static_asset_response(asset: StaticAsset) -> Response {
    let (body, content_type) = asset_bytes(asset);
    response_with_body(StatusCode::OK, content_type, body)
}

fn json_response<T: Serialize>(status: StatusCode, payload: &T) -> Response {
    match serde_json::to_vec(payload) {
        Ok(body) => response_with_body(status, "application/json", Bytes::from(body)),
        Err(_) => response_with_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            Bytes::from_static(br#"{"error":"failed to serialize json response"}"#),
        ),
    }
}

fn html_response(status: StatusCode, body: String) -> Response {
    response_with_body(status, "text/html; charset=utf-8", Bytes::from(body))
}

fn redirect_response(location: &str, set_cookie: Option<String>) -> Response {
    let mut response =
        response_with_body(StatusCode::FOUND, "text/plain; charset=utf-8", Bytes::new());
    set_header_str(&mut response, LOCATION.as_str(), location);
    if let Some(set_cookie) = set_cookie {
        set_header_str(&mut response, SET_COOKIE.as_str(), &set_cookie);
    }
    response
}

fn empty_response(status: StatusCode) -> Response {
    response_with_body(status, "text/plain; charset=utf-8", Bytes::new())
}

fn response_with_body(status: StatusCode, content_type: &str, body: Bytes) -> Response {
    let length = body.len().to_string();
    let mut response = Response::new(warp::hyper::Body::from(body));
    *response.status_mut() = status;
    set_header_str(&mut response, CONTENT_TYPE.as_str(), content_type);
    set_header_str(&mut response, CONTENT_LENGTH.as_str(), &length);
    response
}

fn set_header_str(response: &mut Response, name: &str, value: &str) {
    let Ok(name) = HeaderName::try_from(name) else {
        return;
    };
    let Ok(value) = HeaderValue::from_str(value) else {
        return;
    };
    response.headers_mut().insert(name, value);
}

fn sse_response(events: Vec<Value>) -> Response {
    let stream = futures_util::stream::iter(events.into_iter().map(|event| {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_string();
        let data = event.to_string();
        Ok::<Event, Infallible>(Event::default().event(kind).data(data))
    }));
    let mut response = warp::sse::reply(warp::sse::keep_alive().stream(stream)).into_response();
    set_header_str(&mut response, CACHE_CONTROL.as_str(), "no-cache");
    set_header_str(&mut response, "connection", "close");
    response
}

fn websocket_error_json(message: &str) -> String {
    json!({
        "type": "error",
        "status": 400,
        "error": {
            "type": "invalid_request_error",
            "message": message,
        }
    })
    .to_string()
}

enum TaskDetailsPath {
    Details { task_id: String },
    SiblingTurns { task_id: String, turn_id: String },
}

fn parse_task_details_path(path: &str) -> Option<TaskDetailsPath> {
    const PREFIXES: [&str; 3] = [
        "/api/codex/tasks/",
        "/wham/tasks/",
        "/backend-api/wham/tasks/",
    ];

    for prefix in PREFIXES {
        let Some(remainder) = path.strip_prefix(prefix) else {
            continue;
        };
        let parts: Vec<&str> = remainder
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() == 1 {
            return Some(TaskDetailsPath::Details {
                task_id: parts[0].to_string(),
            });
        }
        if parts.len() == 4 && parts[1] == "turns" && parts[3] == "sibling_turns" {
            return Some(TaskDetailsPath::SiblingTurns {
                task_id: parts[0].to_string(),
                turn_id: parts[2].to_string(),
            });
        }
    }
    None
}

fn parse_social_login_callback_path(path: &str) -> Option<&str> {
    let remainder = path.strip_prefix("/oauth/login/")?;
    let provider_id = remainder.strip_suffix("/callback")?;
    if provider_id.is_empty() || provider_id.contains('/') {
        None
    } else {
        Some(provider_id)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use clap::Parser;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::server::MockServerArgs;
    use crate::server::config::load_login_ui_config;
    use crate::server::responses_proxy::ResponsesProxy;

    fn test_filter() -> HttpRoute {
        test_filter_with_args(MockServerArgs::parse_from(["mock-server"]))
    }

    fn test_filter_with_args(args: MockServerArgs) -> HttpRoute {
        let login_ui_config =
            load_login_ui_config(args.social_login_config.as_deref()).expect("login ui config");
        let responses_proxy = ResponsesProxy::load(
            args.social_login_config.as_deref(),
            args.responses_upstream_base_url.as_deref(),
            args.responses_upstream_api_key.as_deref(),
        )
        .expect("responses proxy config");
        let state = AppState::new(args, login_ui_config, responses_proxy);
        routes(state)
    }

    fn write_social_login_config(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mock-chatgpt-account-server-social-login-{}.toml",
            Uuid::new_v4()
        ));
        fs::write(&path, contents).expect("write social login config");
        path
    }

    fn remove_social_login_config(path: &PathBuf) {
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let args = MockServerArgs::parse_from(["mock-server"]);
        let filter = test_filter_with_args(args);
        let response = warp::test::request().path("/healthz").reply(&filter).await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("healthz json");
        assert_eq!(body, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn remote_control_console_page_returns_bootstrap_payload() {
        let filter = test_filter();
        let response = warp::test::request()
            .path("/codex/remote-control")
            .header("Accept", "application/json")
            .reply(&filter)
            .await;
        let body =
            serde_json::from_slice::<Value>(response.body()).expect("remote-control bootstrap");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body.get("page").and_then(Value::as_str),
            Some("remoteControl"),
        );
        assert_eq!(
            body.get("protocolVersion").and_then(Value::as_str),
            Some("3"),
        );
        // backendBaseUrl is built from the request host; default test host is
        // "127.0.0.1:8765".
        assert!(
            body.get("backendBaseUrl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .ends_with("/backend-api"),
            "backendBaseUrl should end with /backend-api"
        );
        assert!(
            body.get("suggestedInstallationId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .starts_with("install-"),
            "suggestedInstallationId should start with 'install-'"
        );
        assert_eq!(
            body.get("strictAccountHeader").and_then(Value::as_bool),
            Some(false),
        );
    }

    #[tokio::test]
    async fn device_page_json_lists_created_codes() {
        let filter = test_filter();
        let created = warp::test::request()
            .method("POST")
            .path("/api/accounts/deviceauth/usercode")
            .reply(&filter)
            .await;
        let created = serde_json::from_slice::<Value>(created.body()).expect("create json");
        let user_code = created
            .get("user_code")
            .and_then(Value::as_str)
            .expect("user_code");

        let device_page = warp::test::request()
            .path("/codex/device")
            .header("Accept", "application/json")
            .reply(&filter)
            .await;
        let device_page =
            serde_json::from_slice::<Value>(device_page.body()).expect("device page json");

        assert_eq!(
            device_page.get("page"),
            Some(&Value::String("deviceAuth".to_string()))
        );
        let records = device_page
            .get("records")
            .and_then(Value::as_array)
            .expect("records array");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("userCode"),
            Some(&Value::String(user_code.to_string()))
        );
    }

    #[tokio::test]
    async fn authorize_bootstrap_preserves_full_continue_to_query_when_not_logged_in() {
        let filter = test_filter();
        let response = warp::test::request()
            .path(
                "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state",
            )
            .header("Accept", "application/json")
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("authorize json");

        assert_eq!(
            body,
            json!({
                "page": "browserLogin",
                "continueTo": "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state",
                "usernameHint": "debug@example.com",
                "passwordHint": "debug-password",
                "errorMessage": Value::Null,
                "socialProviders": [],
            })
        );
    }

    #[tokio::test]
    async fn authorize_bootstrap_includes_social_login_providers_from_toml() {
        let config_path = write_social_login_config(
            r#"
[social_login.google]
subtitle = "debug@example.com"
client_id = "google-client"
client_secret = "google-secret"

[social_login.github]
subtitle = "@debug-codex"
client_id = "github-client"
client_secret = "github-secret"
"#,
        );
        let filter = test_filter_with_args(MockServerArgs::parse_from([
            "mock-server",
            "--social-login-config",
            config_path.to_str().expect("utf-8 path"),
        ]));
        let response = warp::test::request()
            .path(
                "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state",
            )
            .header("Accept", "application/json")
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("authorize json");

        assert_eq!(
            body.get("socialProviders"),
            Some(&json!([
                {
                    "id": "google",
                    "label": "Continue with Google",
                    "subtitle": "debug@example.com",
                },
                {
                    "id": "github",
                    "label": "Continue with GitHub",
                    "subtitle": "@debug-codex",
                },
            ]))
        );
        remove_social_login_config(&config_path);
    }

    #[tokio::test]
    async fn authorize_shows_account_confirm_page_when_logged_in() {
        let filter = test_filter();
        let login_response = warp::test::request()
            .method("POST")
            .path("/oauth/login")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(
                "username=debug%40example.com&password=debug-password&continue_to=%2Foauth%2Fauthorize%3Fredirect_uri%3Dhttp%253A%252F%252F127.0.0.1%253A9999%252Fcallback%26state%3Dtest-state",
            )
            .reply(&filter)
            .await;
        let session_cookie = login_response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .expect("set-cookie")
            .to_string();

        let response = warp::test::request()
            .path(
                "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state",
            )
            .header("Accept", "application/json")
            .header("Cookie", session_cookie)
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("authorize json");

        assert_eq!(
            body,
            json!({
                "page": "accountConfirm",
                "continueTo": "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state",
                "email": "debug@example.com",
                "accountId": "org-debug",
                "planType": "pro",
                "organizationId": "org-debug",
                "projectId": "",
                "redirectUri": "http://127.0.0.1:9999/callback",
                "oauthState": "test-state",
            })
        );
    }

    #[tokio::test]
    async fn authorize_confirm_page_uses_logged_in_browser_session_username() {
        let filter = test_filter_with_args(MockServerArgs::parse_from([
            "mock-server",
            "--email",
            "configured@example.com",
            "--login-username",
            "google-user@example.com",
        ]));
        let login_response = warp::test::request()
            .method("POST")
            .path("/oauth/login")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(
                "username=google-user%40example.com&password=debug-password&continue_to=%2Foauth%2Fauthorize%3Fredirect_uri%3Dhttp%253A%252F%252F127.0.0.1%253A9999%252Fcallback%26state%3Dtest-state",
            )
            .reply(&filter)
            .await;
        let session_cookie = login_response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .expect("set-cookie")
            .to_string();

        let response = warp::test::request()
            .path(
                "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state",
            )
            .header("Accept", "application/json")
            .header("Cookie", session_cookie)
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("authorize json");

        assert_eq!(
            body.get("email"),
            Some(&Value::String("google-user@example.com".to_string()))
        );
    }

    #[tokio::test]
    async fn oauth_token_refresh_accepts_json_requests() {
        let filter = test_filter();
        let response = warp::test::request()
            .method("POST")
            .path("/oauth/token")
            .header("Content-Type", "application/json")
            .body(r#"{"grant_type":"refresh_token","client_id":"app_EMoamEEZ73f0CkXaXp7hrann","refresh_token":"mock-chatgpt-refresh-token"}"#)
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("oauth token json");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body.get("refresh_token"),
            Some(&Value::String("mock-chatgpt-refresh-token".to_string()))
        );
        assert!(body.get("id_token").and_then(Value::as_str).is_some());
        assert!(body.get("access_token").and_then(Value::as_str).is_some());
    }

    #[tokio::test]
    async fn oauth_token_authorization_code_accepts_form_requests() {
        let filter = test_filter();
        let response = warp::test::request()
            .method("POST")
            .path("/oauth/token")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("grant_type=authorization_code&code=auth-code")
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("oauth token json");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body.get("refresh_token"),
            Some(&Value::String("mock-chatgpt-refresh-token".to_string()))
        );
        assert!(body.get("id_token").and_then(Value::as_str).is_some());
        assert!(body.get("access_token").and_then(Value::as_str).is_some());
    }

    #[tokio::test]
    async fn browser_login_json_redirect_preserves_full_continue_to_query() {
        let filter = test_filter();
        let continue_to = "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state";
        let encoded_continue_to =
            url::form_urlencoded::byte_serialize(continue_to.as_bytes()).collect::<String>();
        let response = warp::test::request()
            .method("POST")
            .path("/oauth/login")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "username=debug%40example.com&password=debug-password&continue_to={encoded_continue_to}"
            ))
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("login json");

        assert_eq!(
            body,
            json!({
                "ok": true,
                "redirectTo": continue_to,
            })
        );
        assert!(
            response
                .headers()
                .get("set-cookie")
                .and_then(|value| value.to_str().ok())
                .map(|cookie| cookie.contains(BROWSER_SESSION_COOKIE))
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn shortcut_login_redirects_to_provider_authorize_url() {
        let config_path = write_social_login_config(
            r#"
[social_login.google]
subtitle = "debug@example.com"
client_id = "google-client"
client_secret = "google-secret"
authorize_url = "https://provider.example/oauth/authorize"
token_url = "https://provider.example/oauth/token"
user_info_url = "https://provider.example/oauth/userinfo"
"#,
        );
        let filter = test_filter_with_args(MockServerArgs::parse_from([
            "mock-server",
            "--social-login-config",
            config_path.to_str().expect("utf-8 path"),
        ]));
        let continue_to = "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state";
        let encoded_continue_to =
            url::form_urlencoded::byte_serialize(continue_to.as_bytes()).collect::<String>();
        let response = warp::test::request()
            .method("POST")
            .path("/oauth/login/shortcut")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("provider=google&continue_to={encoded_continue_to}"))
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("shortcut login json");
        let redirect_to = body
            .get("redirectTo")
            .and_then(Value::as_str)
            .expect("redirectTo");
        let redirect_url = url::Url::parse(redirect_to).expect("authorize redirect url");
        let redirect_params = redirect_url
            .query_pairs()
            .into_owned()
            .collect::<HashMap<String, String>>();

        assert_eq!(body.get("ok"), Some(&Value::Bool(true)));
        assert!(redirect_to.starts_with("https://provider.example/oauth/authorize"));
        assert_eq!(
            redirect_params.get("client_id"),
            Some(&"google-client".to_string())
        );
        assert_eq!(
            redirect_params.get("redirect_uri"),
            Some(&"http://127.0.0.1:8765/oauth/login/google/callback".to_string())
        );
        assert_eq!(
            redirect_params.get("scope"),
            Some(&"openid email profile".to_string())
        );
        assert!(redirect_params.contains_key("state"));
        remove_social_login_config(&config_path);
    }

    #[tokio::test]
    async fn shortcut_login_callback_requires_authorization_code() {
        let config_path = write_social_login_config(
            r#"
[social_login.google]
subtitle = "debug@example.com"
client_id = "google-client"
client_secret = "google-secret"
authorize_url = "https://provider.example/oauth/authorize"
token_url = "https://provider.example/oauth/token"
user_info_url = "https://provider.example/oauth/userinfo"
"#,
        );
        let filter = test_filter_with_args(MockServerArgs::parse_from([
            "mock-server",
            "--social-login-config",
            config_path.to_str().expect("utf-8 path"),
        ]));
        let continue_to = "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state";
        let encoded_continue_to =
            url::form_urlencoded::byte_serialize(continue_to.as_bytes()).collect::<String>();
        let login_response = warp::test::request()
            .method("POST")
            .path("/oauth/login/shortcut")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("provider=google&continue_to={encoded_continue_to}"))
            .reply(&filter)
            .await;
        let login_body =
            serde_json::from_slice::<Value>(login_response.body()).expect("shortcut login json");
        let redirect_to = login_body
            .get("redirectTo")
            .and_then(Value::as_str)
            .expect("redirectTo");
        let redirect_url = url::Url::parse(redirect_to).expect("authorize redirect url");
        let redirect_params = redirect_url
            .query_pairs()
            .into_owned()
            .collect::<HashMap<String, String>>();
        let oauth_state = redirect_params.get("state").expect("oauth state");

        let callback_response = warp::test::request()
            .header("Accept", "application/json")
            .path(&format!("/oauth/login/google/callback?state={oauth_state}"))
            .reply(&filter)
            .await;
        let callback_body =
            serde_json::from_slice::<Value>(callback_response.body()).expect("callback json");

        assert_eq!(callback_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            callback_body.get("page"),
            Some(&Value::String("browserLogin".to_string()))
        );
        assert_eq!(
            callback_body.get("continueTo"),
            Some(&Value::String(continue_to.to_string()))
        );
        assert!(
            callback_body
                .get("errorMessage")
                .and_then(Value::as_str)
                .map(|message| message.contains("missing the authorization code"))
                .unwrap_or(false)
        );
        remove_social_login_config(&config_path);
    }

    #[tokio::test]
    async fn authorize_approve_redirects_to_callback_after_confirmation() {
        let filter = test_filter();
        let continue_to = "/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback&state=test-state";
        let encoded_continue_to =
            url::form_urlencoded::byte_serialize(continue_to.as_bytes()).collect::<String>();
        let login_response = warp::test::request()
            .method("POST")
            .path("/oauth/login")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "username=debug%40example.com&password=debug-password&continue_to={encoded_continue_to}"
            ))
            .reply(&filter)
            .await;
        let session_cookie = login_response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .expect("set-cookie")
            .to_string();

        let response = warp::test::request()
            .method("POST")
            .path("/oauth/authorize/approve")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Cookie", session_cookie)
            .body(format!("continue_to={encoded_continue_to}"))
            .reply(&filter)
            .await;
        let body = serde_json::from_slice::<Value>(response.body()).expect("approve json");
        let redirect_to = body
            .get("redirectTo")
            .and_then(Value::as_str)
            .expect("redirectTo");

        assert_eq!(body.get("ok"), Some(&Value::Bool(true)));
        assert!(redirect_to.starts_with("http://127.0.0.1:9999/callback?code=auth-"));
        assert!(redirect_to.ends_with("&state=test-state"));
    }

    #[tokio::test]
    async fn response_events_include_completion_sequence() {
        let args = MockServerArgs::parse_from(["mock-server"]);
        let state = AppState::new(args, Default::default(), ResponsesProxy::default());
        let event_types = build_responses_events(
            &state,
            &json!({
                "type": "response.create",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            { "type": "input_text", "text": "hello mock server" }
                        ]
                    }
                ]
            }),
        )
        .into_iter()
        .filter_map(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();

        assert_eq!(
            event_types,
            vec![
                "response.created".to_string(),
                "response.output_item.added".to_string(),
                "response.output_text.delta".to_string(),
                "response.output_item.done".to_string(),
                "response.completed".to_string(),
            ]
        );
    }
}
