use clap::Parser;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Debug, Parser)]
#[command(about = "Local mock server for Codex ChatGPT account flows.")]
pub struct MockServerArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    #[arg(long, default_value_t = 8765)]
    pub port: u16,

    #[arg(long, default_value = "debug@example.com")]
    pub email: String,

    #[arg(long)]
    pub login_username: Option<String>,

    #[arg(long, default_value = "debug-password")]
    pub login_password: String,

    #[arg(
        long,
        visible_alias = "server-config",
        value_name = "FILE",
        help = "Optional TOML file that configures Google/GitHub shortcut login buttons and responses proxy upstreams."
    )]
    pub social_login_config: Option<PathBuf>,

    #[arg(long, default_value = "pro")]
    pub plan_type: String,

    #[arg(long, default_value = "org-debug")]
    pub chatgpt_account_id: String,

    #[arg(long, default_value = "user-debug")]
    pub chatgpt_user_id: String,

    #[arg(long)]
    pub organization_id: Option<String>,

    #[arg(long, default_value = "")]
    pub project_id: String,

    #[arg(
        long = "no-completed-platform-onboarding",
        action = clap::ArgAction::SetFalse,
        default_value_t = true
    )]
    pub completed_platform_onboarding: bool,

    #[arg(long, default_value_t = false)]
    pub is_org_owner: bool,

    #[arg(
        long,
        default_value = "mock-chatgpt-access-token",
        help = "Opaque identifier stored in the mock JWT access token jti claim."
    )]
    pub access_token: String,

    #[arg(long, default_value = "mock-chatgpt-refresh-token")]
    pub refresh_token: String,

    #[arg(
        long,
        default_value = "sk-mock-api-key",
        help = "Local mock API key returned by token exchange and required by backend-api endpoints."
    )]
    pub api_key: String,

    #[arg(
        long,
        value_name = "URL",
        requires = "responses_upstream_api_key",
        help = "Legacy single upstream OpenAI-compatible base URL used to proxy responses requests. Accepts either .../v1 or a full .../responses URL."
    )]
    pub responses_upstream_base_url: Option<String>,

    #[arg(
        long,
        value_name = "KEY",
        requires = "responses_upstream_base_url",
        help = "Legacy single upstream API key used when proxying responses requests."
    )]
    pub responses_upstream_api_key: Option<String>,

    #[arg(
        long,
        default_value = "Mock response from {model} for: {prompt}",
        help = "Template used by /backend-api/codex/responses and generated task details."
    )]
    pub responses_output_text: String,

    #[arg(
        long,
        default_value = "# mock requirements\n[network]\nallowed_domains = [\"api.openai.com\", \"chatgpt.com\"]\n",
        help = "Contents returned by the mock requirements endpoint."
    )]
    pub requirements_contents: String,

    #[arg(
        long,
        default_value = "Mock task",
        help = "Fallback prefix used when generated task titles have no prompt text."
    )]
    pub task_title_prefix: String,

    #[arg(long, default_value = "gpt-5.3-codex")]
    pub model_slug: String,

    #[arg(long, default_value = "gpt-5.3-codex")]
    pub model_display_name: String,

    #[arg(
        long,
        default_value = "Mock remote model served by the local ChatGPT account server."
    )]
    pub model_description: String,

    #[arg(long, default_value = "medium")]
    pub model_default_reasoning_level: String,

    #[arg(long, default_value_t = 0)]
    pub model_priority: i32,

    #[arg(long, default_value_t = 272_000)]
    pub model_context_window: i32,

    #[arg(long, default_value_t = 10_000)]
    pub model_truncation_limit: i32,

    #[arg(long, default_value = "mock-models-etag-v1")]
    pub models_etag: String,

    #[arg(long, default_value_t = 42)]
    pub primary_used_percent: i32,

    #[arg(long, default_value_t = 60)]
    pub primary_window_mins: i32,

    #[arg(long, default_value_t = 120)]
    pub primary_resets_in_secs: i64,

    #[arg(long, default_value_t = 5)]
    pub secondary_used_percent: i32,

    #[arg(long, default_value_t = 1440)]
    pub secondary_window_mins: i32,

    #[arg(long, default_value_t = 43_200)]
    pub secondary_resets_in_secs: i64,

    #[arg(
        long = "additional-limit",
        value_name = "LIMIT_ID:USED_PERCENT:WINDOW_MINS:RESETS_IN_SECS[:LIMIT_NAME]"
    )]
    pub additional_limit: Vec<LimitBucket>,

    #[arg(long, default_value_t = 1)]
    pub device_code_interval_secs: u64,

    #[arg(long, default_value_t = 1)]
    pub device_code_pending_polls: u32,

    #[arg(long, default_value_t = false)]
    pub device_code_auto_approve: bool,

    #[arg(long, default_value_t = false)]
    pub strict_account_header: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LimitBucket {
    pub limit_id: String,
    pub used_percent: i32,
    pub window_mins: i32,
    pub resets_in_secs: i64,
    pub limit_name: Option<String>,
}

impl LimitBucket {
    pub fn as_usage_payload(&self, now_unix: i64) -> Value {
        json!({
            "limit_name": self.limit_name.as_deref().unwrap_or(self.limit_id.as_str()),
            "metered_feature": self.limit_id,
            "rate_limit": {
                "allowed": true,
                "limit_reached": self.used_percent >= 100,
                "primary_window": {
                    "used_percent": self.used_percent,
                    "limit_window_seconds": self.window_mins * 60,
                    "reset_after_seconds": self.resets_in_secs,
                    "reset_at": now_unix + self.resets_in_secs,
                },
            },
        })
    }
}

impl FromStr for LimitBucket {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = raw.split(':').collect();
        if parts.len() != 4 && parts.len() != 5 {
            return Err(
                "additional limit must be LIMIT_ID:USED_PERCENT:WINDOW_MINS:RESETS_IN_SECS[:LIMIT_NAME]"
                    .to_string(),
            );
        }

        Ok(Self {
            limit_id: parts[0].to_string(),
            used_percent: parts[1]
                .parse()
                .map_err(|_| "used_percent must be an integer".to_string())?,
            window_mins: parts[2]
                .parse()
                .map_err(|_| "window_mins must be an integer".to_string())?,
            resets_in_secs: parts[3]
                .parse()
                .map_err(|_| "resets_in_secs must be an integer".to_string())?,
            limit_name: parts.get(4).map(|value| (*value).to_string()),
        })
    }
}

impl MockServerArgs {
    pub fn responses_proxy_enabled(&self) -> bool {
        self.responses_upstream_base_url.is_some()
    }
}
