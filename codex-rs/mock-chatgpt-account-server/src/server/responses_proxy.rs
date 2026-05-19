use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use url::Url;
use warp::http::HeaderMap;
use warp::http::StatusCode;
use warp::reply::Response;

use crate::server::state::AppState;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesProxyUpstreamConfig {
    pub name: String,
    pub endpoint_url: String,
    pub api_key: String,
}

#[derive(Clone, Default)]
pub struct ResponsesProxy {
    inner: Option<Arc<ResponsesProxyInner>>,
}

struct ResponsesProxyInner {
    client: OnceLock<reqwest::Client>,
    upstreams: Vec<ResponsesProxyUpstreamConfig>,
    next_upstream: AtomicUsize,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ResponsesProxyFile {
    responses_proxy: ResponsesProxyTable,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ResponsesProxyTable {
    upstreams: Vec<ConfiguredResponsesProxyUpstream>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct ConfiguredResponsesProxyUpstream {
    enabled: bool,
    name: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

impl Default for ConfiguredResponsesProxyUpstream {
    fn default() -> Self {
        Self {
            enabled: true,
            name: None,
            base_url: None,
            api_key: None,
        }
    }
}

impl ResponsesProxy {
    pub fn load(
        config_path: Option<&Path>,
        legacy_base_url: Option<&str>,
        legacy_api_key: Option<&str>,
    ) -> Result<Self> {
        let mut upstreams = Vec::new();

        if let Some(config_path) = config_path {
            let raw = fs::read_to_string(config_path).with_context(|| {
                format!(
                    "failed to read responses proxy config {}",
                    config_path.display()
                )
            })?;
            upstreams.extend(Self::parse_upstreams_from_toml(&raw).with_context(|| {
                format!(
                    "failed to parse responses proxy config {}",
                    config_path.display()
                )
            })?);
        }

        match (legacy_base_url, legacy_api_key) {
            (Some(base_url), Some(api_key)) => upstreams.push(build_upstream_config(
                Some("legacy-cli-upstream".to_string()),
                Some(base_url.to_string()),
                Some(api_key.to_string()),
                "legacy responses proxy options",
            )?),
            (None, None) => {}
            _ => bail!("responses proxy legacy options require both base_url and api_key"),
        }

        Self::new(upstreams)
    }

    fn new(upstreams: Vec<ResponsesProxyUpstreamConfig>) -> Result<Self> {
        if upstreams.is_empty() {
            return Ok(Self::default());
        }

        Ok(Self {
            inner: Some(Arc::new(ResponsesProxyInner {
                client: OnceLock::new(),
                upstreams,
                next_upstream: AtomicUsize::new(0),
            })),
        })
    }

    fn parse_upstreams_from_toml(raw: &str) -> Result<Vec<ResponsesProxyUpstreamConfig>> {
        let parsed: ResponsesProxyFile = toml::from_str(raw)?;
        parsed
            .responses_proxy
            .upstreams
            .into_iter()
            .enumerate()
            .map(|(index, upstream)| {
                if !upstream.enabled {
                    return Ok(None);
                }

                let source = format!("responses_proxy.upstreams[{index}]");
                build_upstream_config(upstream.name, upstream.base_url, upstream.api_key, &source)
                    .map(Some)
            })
            .collect::<Result<Vec<_>>>()
            .map(|upstreams| upstreams.into_iter().flatten().collect())
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn upstream_labels(&self) -> Vec<String> {
        self.inner
            .as_ref()
            .map(|inner| {
                inner
                    .upstreams
                    .iter()
                    .map(|upstream| format!("{}={}", upstream.name, upstream.endpoint_url))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn maybe_proxy_request(&self, headers: &HeaderMap, body: &Bytes) -> Option<Response> {
        if !self.is_enabled() {
            return None;
        }

        Some(match self.forward_request(headers, body.clone()).await {
            Ok(response) => response,
            Err(error) => json_error_response(
                StatusCode::BAD_GATEWAY,
                format!("failed to proxy responses request: {error}"),
            ),
        })
    }

    async fn forward_request(&self, headers: &HeaderMap, body: Bytes) -> Result<Response> {
        let Some(inner) = &self.inner else {
            bail!("responses proxy is not configured");
        };
        let upstream_count = inner.upstreams.len();
        let start_index = next_upstream_index(inner, upstream_count);
        let mut last_error: Option<anyhow::Error> = None;

        for offset in 0..upstream_count {
            let upstream = &inner.upstreams[(start_index + offset) % upstream_count];
            println!(
                "[mock-account-server] proxying /responses to upstream={} endpoint={}",
                upstream.name, upstream.endpoint_url
            );

            match forward_request_to_upstream(inner, upstream, headers, body.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    println!(
                        "[mock-account-server] upstream={} transport failure: {error}",
                        upstream.name
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("responses proxy has no usable upstreams")))
    }
}

pub async fn maybe_proxy_responses_request(
    state: &AppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Option<Response> {
    state
        .responses_proxy()
        .maybe_proxy_request(headers, body)
        .await
}

async fn forward_request_to_upstream(
    inner: &ResponsesProxyInner,
    upstream: &ResponsesProxyUpstreamConfig,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let client = proxy_client(inner);
    let mut request = client.post(&upstream.endpoint_url).body(body);

    for (name, value) in headers {
        if !should_forward_request_header(name.as_str()) {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        request = request.header(name.as_str(), value);
    }

    let upstream_response = request
        .header("authorization", format!("Bearer {}", upstream.api_key))
        .send()
        .await
        .with_context(|| format!("failed to reach upstream {}", upstream.name))?;

    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = upstream_response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            if !should_forward_response_header(name.as_str()) {
                return None;
            }
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect::<Vec<_>>();
    let stream = futures_util::stream::try_unfold(upstream_response, |mut response| async {
        match response.chunk().await {
            Ok(Some(chunk)) => Ok(Some((chunk, response))),
            Ok(None) => Ok(None),
            Err(error) => Err(std::io::Error::other(error)),
        }
    });

    let mut response = Response::new(warp::hyper::Body::wrap_stream(stream));
    *response.status_mut() = status;
    for (name, value) in response_headers {
        set_header_str(&mut response, &name, &value);
    }
    println!(
        "[mock-account-server] upstream={} returned {}",
        upstream.name, status
    );
    Ok(response)
}

fn proxy_client(inner: &ResponsesProxyInner) -> &reqwest::Client {
    inner.client.get_or_init(reqwest::Client::new)
}

fn next_upstream_index(inner: &ResponsesProxyInner, upstream_count: usize) -> usize {
    inner.next_upstream.fetch_add(1, Ordering::Relaxed) % upstream_count
}

fn build_upstream_config(
    name: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    source: &str,
) -> Result<ResponsesProxyUpstreamConfig> {
    let endpoint_url =
        normalize_endpoint_url(required_trimmed_string(base_url, source, "base_url")?)?;

    Ok(ResponsesProxyUpstreamConfig {
        name: trim_to_option(name).unwrap_or_else(|| endpoint_url.clone()),
        endpoint_url,
        api_key: required_trimmed_string(api_key, source, "api_key")?,
    })
}

fn normalize_endpoint_url(base_url: String) -> Result<String> {
    let mut url =
        Url::parse(&base_url).with_context(|| format!("invalid upstream base_url: {base_url}"))?;
    if url.query().is_some() {
        bail!("responses proxy upstream base_url must not contain a query string: {base_url}");
    }
    if url.fragment().is_some() {
        bail!("responses proxy upstream base_url must not contain a fragment: {base_url}");
    }

    let trimmed_path = url.path().trim_end_matches('/');
    let normalized_path = if trimmed_path.ends_with("/responses") {
        if trimmed_path.is_empty() {
            "/responses".to_string()
        } else {
            trimmed_path.to_string()
        }
    } else if trimmed_path.is_empty() {
        "/responses".to_string()
    } else {
        format!("{trimmed_path}/responses")
    };
    url.set_path(&normalized_path);
    Ok(url.into())
}

fn required_trimmed_string(
    value: Option<String>,
    source: &str,
    field_name: &str,
) -> Result<String> {
    trim_to_option(value).with_context(|| format!("{source} requires a non-empty {field_name}"))
}

fn trim_to_option(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn should_forward_request_header(name: &str) -> bool {
    !matches!(
        name,
        "authorization"
            | "chatgpt-account-id"
            | "connection"
            | "content-length"
            | "cookie"
            | "host"
            | "transfer-encoding"
    )
}

fn should_forward_response_header(name: &str) -> bool {
    !matches!(name, "connection" | "content-length" | "transfer-encoding")
}

fn json_error_response(status: StatusCode, message: String) -> Response {
    let body = serde_json::to_vec(&json!({ "error": message }))
        .unwrap_or_else(|_| br#"{"error":"failed to serialize proxy error"}"#.to_vec());
    let mut response = Response::new(warp::hyper::Body::from(body.clone()));
    *response.status_mut() = status;
    set_header_str(&mut response, "content-type", "application/json");
    set_header_str(&mut response, "content-length", &body.len().to_string());
    response
}

fn set_header_str(response: &mut Response, name: &str, value: &str) {
    let Ok(name) = warp::http::header::HeaderName::try_from(name) else {
        return;
    };
    let Ok(value) = warp::http::header::HeaderValue::from_str(value) else {
        return;
    };
    response.headers_mut().insert(name, value);
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn parses_upstreams_from_toml_and_normalizes_endpoint_urls() {
        let upstreams = ResponsesProxy::parse_upstreams_from_toml(
            r#"
[[responses_proxy.upstreams]]
name = "primary"
base_url = "https://api.openai.example/v1"
api_key = "key-1"

[[responses_proxy.upstreams]]
base_url = "https://api.openai.example/custom/responses"
api_key = "key-2"
"#,
        )
        .expect("responses proxy config");

        assert_eq!(
            upstreams,
            vec![
                ResponsesProxyUpstreamConfig {
                    name: "primary".to_string(),
                    endpoint_url: "https://api.openai.example/v1/responses".to_string(),
                    api_key: "key-1".to_string(),
                },
                ResponsesProxyUpstreamConfig {
                    name: "https://api.openai.example/custom/responses".to_string(),
                    endpoint_url: "https://api.openai.example/custom/responses".to_string(),
                    api_key: "key-2".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn selects_upstreams_round_robin() {
        let proxy = ResponsesProxy::new(vec![
            ResponsesProxyUpstreamConfig {
                name: "first".to_string(),
                endpoint_url: "https://first.example/v1/responses".to_string(),
                api_key: "key-first".to_string(),
            },
            ResponsesProxyUpstreamConfig {
                name: "second".to_string(),
                endpoint_url: "https://second.example/v1/responses".to_string(),
                api_key: "key-second".to_string(),
            },
        ])
        .expect("responses proxy");

        assert_eq!(
            proxy.next_upstream_name_for_test(),
            Some("first".to_string())
        );
        assert_eq!(
            proxy.next_upstream_name_for_test(),
            Some("second".to_string())
        );
        assert_eq!(
            proxy.next_upstream_name_for_test(),
            Some("first".to_string())
        );
    }

    #[tokio::test]
    async fn load_merges_toml_and_legacy_cli_upstreams() {
        let config_path = std::env::temp_dir().join(format!(
            "mock-chatgpt-account-server-proxy-{}.toml",
            Uuid::new_v4()
        ));
        fs::write(
            &config_path,
            r#"
[[responses_proxy.upstreams]]
name = "toml"
base_url = "https://example.com/v1"
api_key = "toml-key"
"#,
        )
        .expect("write config");

        let proxy = ResponsesProxy::load(
            Some(&config_path),
            Some("https://legacy.example/v1"),
            Some("legacy-key"),
        )
        .expect("responses proxy");

        assert_eq!(
            proxy.upstream_labels(),
            vec![
                "toml=https://example.com/v1/responses".to_string(),
                "legacy-cli-upstream=https://legacy.example/v1/responses".to_string(),
            ]
        );

        let _ = fs::remove_file(config_path);
    }

    impl ResponsesProxy {
        fn next_upstream_name_for_test(&self) -> Option<String> {
            let inner = self.inner.as_ref()?;
            let index = next_upstream_index(inner, inner.upstreams.len());
            Some(inner.upstreams[index].name.clone())
        }
    }
}
