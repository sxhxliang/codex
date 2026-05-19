mod args;
mod config;
mod frontend;
mod remote_control;
mod responses;
mod responses_proxy;
mod routes;
mod state;

use std::net::SocketAddr;

use anyhow::Context;
use anyhow::Result;

pub use args::MockServerArgs;
use config::load_login_ui_config;
use responses_proxy::ResponsesProxy;
use state::AppState;

pub mod testing {
    //! Test-only helpers for spinning up the mock server in integration tests.
    //! These intentionally bypass `MockServerArgs` resolution so tests can
    //! bind to an ephemeral port and learn the bound address.

    use std::net::SocketAddr;

    use anyhow::Result;
    use tokio::task::JoinHandle;

    use super::AppState;
    use super::MockServerArgs;
    use super::config::load_login_ui_config;
    use super::responses_proxy::ResponsesProxy;
    use super::routes;

    /// Spawn the mock server on an ephemeral port; return the bound address
    /// plus a handle to the server task. The task ends when the runtime is
    /// dropped or the listener errors.
    pub async fn spawn(args: MockServerArgs) -> Result<(SocketAddr, JoinHandle<()>)> {
        let login_ui_config = load_login_ui_config(args.social_login_config.as_deref())?;
        let responses_proxy = ResponsesProxy::load(
            args.social_login_config.as_deref(),
            args.responses_upstream_base_url.as_deref(),
            args.responses_upstream_api_key.as_deref(),
        )?;
        let state = AppState::new(args, login_ui_config, responses_proxy);
        let bind_addr: SocketAddr = "127.0.0.1:0"
            .parse()
            .map_err(|err| anyhow::anyhow!("invalid bind address: {err}"))?;
        let (addr, server) = warp::serve(routes::routes(state)).bind_ephemeral(bind_addr);
        let handle = tokio::spawn(async move {
            server.await;
        });
        Ok((addr, handle))
    }
}

pub async fn run(args: MockServerArgs) -> Result<()> {
    let bind_addr = resolve_bind_addr(&args).await?;
    let login_ui_config = load_login_ui_config(args.social_login_config.as_deref())?;
    let responses_proxy = ResponsesProxy::load(
        args.social_login_config.as_deref(),
        args.responses_upstream_base_url.as_deref(),
        args.responses_upstream_api_key.as_deref(),
    )?;
    let social_login_providers = login_ui_config
        .social_login_providers()
        .iter()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let responses_proxy_upstreams = responses_proxy.upstream_labels();
    let state = AppState::new(args.clone(), login_ui_config, responses_proxy);
    let login_username = args
        .login_username
        .clone()
        .unwrap_or_else(|| args.email.clone());

    println!(
        "Mock account server listening on http://{}:{}",
        args.host, args.port
    );
    println!("OAuth issuer: http://{}:{}", args.host, args.port);
    println!("Browser login: {login_username} / {}", args.login_password);
    if !social_login_providers.is_empty() {
        println!("Shortcut logins: {social_login_providers}");
    }
    println!(
        "Models endpoints: http://{}:{}/models, /v1/models, and /backend-api/codex/models",
        args.host, args.port
    );
    println!(
        "Responses endpoint: http://{}:{}/backend-api/codex/responses",
        args.host, args.port
    );
    if !responses_proxy_upstreams.is_empty() {
        println!(
            "Responses proxy upstreams: {}",
            responses_proxy_upstreams.join(", ")
        );
    }
    println!(
        "ChatGPT backend base URL: http://{}:{}/backend-api",
        args.host, args.port
    );
    println!(
        "Device auth page: http://{}:{}/codex/device",
        args.host, args.port
    );

    warp::serve(routes::routes(state)).run(bind_addr).await;
    Ok(())
}

async fn resolve_bind_addr(args: &MockServerArgs) -> Result<SocketAddr> {
    tokio::net::lookup_host((args.host.as_str(), args.port))
        .await?
        .next()
        .with_context(|| {
            format!(
                "could not resolve bind address for {}:{}",
                args.host, args.port
            )
        })
}
