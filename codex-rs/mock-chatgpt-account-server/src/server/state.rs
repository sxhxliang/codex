use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::SecondsFormat;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::server::args::MockServerArgs;
use crate::server::config::LoginUiConfig;
use crate::server::config::OAuthSocialLoginProvider;
use crate::server::config::SocialLoginProvider;
use crate::server::remote_control::state::RelayState;
use crate::server::responses_proxy::ResponsesProxy;

pub const BROWSER_SESSION_COOKIE: &str = "mock_codex_browser_session";
pub const CONNECTOR_ID: &str = "calendar";
pub const CONNECTOR_NAME: &str = "Calendar";
pub const CONNECTOR_DESCRIPTION: &str = "Plan events and manage your calendar.";
pub const DISCOVERABLE_CALENDAR_ID: &str = "connector_2128aebfecb84f64a069897515042a44";
pub const DISCOVERABLE_GMAIL_ID: &str = "connector_68df038e0ba48191908c8434991bbac2";
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
pub const MCP_SERVER_NAME: &str = "mock-codex-apps";
pub const MCP_SERVER_VERSION: &str = "1.0.0";
pub const CALENDAR_CREATE_EVENT_RESOURCE_URI: &str =
    "connector://calendar/tools/calendar_create_event";
pub const CALENDAR_LIST_EVENTS_RESOURCE_URI: &str =
    "connector://calendar/tools/calendar_list_events";
pub const APPLY_PATCH_APPROVAL_DEMO_CALL_ID: &str = "call_mock_apply_patch_approval_demo";
pub const APPLY_PATCH_APPROVAL_DEMO_FILE: &str = "APPROVAL_DEMO.txt";
pub const APPLY_PATCH_APPROVAL_DEMO_TRIGGER: &str = "mock apply_patch approval demo";
pub const APPLY_PATCH_APPROVAL_DEMO_FILE_CONTENT: &str =
    "hello from the mock apply_patch approval demo\n";

#[derive(Clone)]
pub struct AppState {
    pub args: MockServerArgs,
    login_ui_config: LoginUiConfig,
    responses_proxy: ResponsesProxy,
    relay: RelayState,
    inner: Arc<RwLock<InnerState>>,
}

#[derive(Default)]
struct InnerState {
    device_codes: HashMap<String, DeviceCodeRecord>,
    auth_codes: HashMap<String, String>,
    browser_sessions: HashMap<String, String>,
    social_login_states: HashMap<String, SocialLoginState>,
    tasks: HashMap<String, TaskRecord>,
}

#[derive(Clone, Debug)]
struct SocialLoginState {
    provider_id: String,
    continue_to: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeRecord {
    pub device_auth_id: String,
    pub user_code: String,
    pub approved: bool,
    pub polls: u32,
    pub authorization_code: String,
    pub code_challenge: String,
    pub code_verifier: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeView {
    pub user_code: String,
    pub approved: bool,
    pub polls: u32,
}

#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub task_id: String,
    pub title: String,
    pub created_at: f64,
    pub updated_at: f64,
    pub current_turn_id: String,
    pub user_turn_id: String,
    pub assistant_turn_id: String,
    pub user_prompt: String,
    pub assistant_response: String,
    pub archived: bool,
    pub has_unread_turn: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPageData {
    pub task_id: String,
    pub title: String,
    pub user_prompt: String,
    pub assistant_response: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementsResponse {
    pub contents: String,
    pub sha256: String,
    pub updated_at: String,
    pub updated_by_user_id: String,
}

pub enum DeviceTokenStatus {
    Unknown,
    Pending,
    Approved {
        authorization_code: String,
        code_challenge: String,
        code_verifier: String,
    },
}

impl AppState {
    pub fn new(
        args: MockServerArgs,
        login_ui_config: LoginUiConfig,
        responses_proxy: ResponsesProxy,
    ) -> Self {
        Self {
            args,
            login_ui_config,
            responses_proxy,
            relay: RelayState::new(),
            inner: Arc::new(RwLock::new(InnerState::default())),
        }
    }

    pub(crate) fn relay(&self) -> &RelayState {
        &self.relay
    }

    pub fn social_login_providers(&self) -> Vec<SocialLoginProvider> {
        self.login_ui_config.social_login_providers()
    }

    pub fn social_login_provider(&self, provider_id: &str) -> Option<&OAuthSocialLoginProvider> {
        self.login_ui_config.social_login_provider(provider_id)
    }

    pub fn responses_proxy(&self) -> &ResponsesProxy {
        &self.responses_proxy
    }

    pub async fn create_social_login_state(&self, provider_id: &str, continue_to: &str) -> String {
        let oauth_state = format!("oauth-state-{}", short_token());
        let mut inner = self.inner.write().await;
        inner.social_login_states.insert(
            oauth_state.clone(),
            SocialLoginState {
                provider_id: provider_id.to_string(),
                continue_to: continue_to.to_string(),
            },
        );
        oauth_state
    }

    pub async fn consume_social_login_continue_to(
        &self,
        provider_id: &str,
        oauth_state: &str,
    ) -> Option<String> {
        let mut inner = self.inner.write().await;
        let state = inner.social_login_states.remove(oauth_state)?;
        if state.provider_id == provider_id {
            Some(state.continue_to)
        } else {
            None
        }
    }

    pub fn build_id_token(&self) -> String {
        fake_jwt(json!({
            "email": self.args.email,
            "https://api.openai.com/profile": {
                "email": self.args.email,
            },
            "https://api.openai.com/auth": self.auth_claims(),
        }))
    }

    pub fn build_access_token(&self) -> String {
        fake_jwt(json!({
            "sub": self.args.chatgpt_user_id,
            "jti": self.args.access_token,
            "https://api.openai.com/auth": self.auth_claims(),
        }))
    }

    pub fn build_models_response(&self) -> Value {
        json!({
            "models": [
                {
                    "slug": self.args.model_slug,
                    "display_name": self.args.model_display_name,
                    "description": self.args.model_description,
                    "default_reasoning_level": self.args.model_default_reasoning_level,
                    "supported_reasoning_levels": [
                        {
                            "effort": "low",
                            "description": "Fast responses with lighter reasoning",
                        },
                        {
                            "effort": "medium",
                            "description": "Balances speed and reasoning depth for everyday tasks",
                        },
                        {
                            "effort": "high",
                            "description": "Greater reasoning depth for complex problems",
                        },
                        {
                            "effort": "xhigh",
                            "description": "Extra high reasoning depth for complex problems",
                        }
                    ],
                    "shell_type": "shell_command",
                    "visibility": "list",
                    "supported_in_api": true,
                    "priority": self.args.model_priority,
                    "availability_nux": Value::Null,
                    "upgrade": Value::Null,
                    "base_instructions": "You are Codex, a coding agent running against the local mock ChatGPT account server.",
                    "supports_reasoning_summaries": true,
                    "default_reasoning_summary": "auto",
                    "support_verbosity": true,
                    "default_verbosity": "low",
                    "apply_patch_tool_type": "freeform",
                    "web_search_tool_type": "text",
                    "truncation_policy": {
                        "mode": "tokens",
                        "limit": self.args.model_truncation_limit,
                    },
                    "supports_parallel_tool_calls": true,
                    "supports_image_detail_original": true,
                    "context_window": self.args.model_context_window,
                    "experimental_supported_tools": [],
                    "input_modalities": ["text", "image"],
                    "prefer_websockets": false,
                    "supports_search_tool": false,
                }
            ]
        })
    }

    pub async fn create_auth_code(&self, prefix: &str) -> String {
        let code = format!("{prefix}-{}", short_token());
        let mut inner = self.inner.write().await;
        inner.auth_codes.insert(code.clone(), iso8601_now());
        code
    }

    pub async fn create_browser_session(&self, username: &str) -> String {
        let session_id = format!("session-{}", short_token());
        let mut inner = self.inner.write().await;
        inner
            .browser_sessions
            .insert(session_id.clone(), username.to_string());
        session_id
    }

    pub async fn has_browser_session(&self, session_id: Option<&str>) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        let inner = self.inner.read().await;
        inner.browser_sessions.contains_key(session_id)
    }

    pub async fn browser_session_username(&self, session_id: Option<&str>) -> Option<String> {
        let session_id = session_id?;
        let inner = self.inner.read().await;
        inner.browser_sessions.get(session_id).cloned()
    }

    pub async fn clear_browser_session(&self, session_id: Option<&str>) {
        let Some(session_id) = session_id else {
            return;
        };
        let mut inner = self.inner.write().await;
        inner.browser_sessions.remove(session_id);
    }

    pub async fn create_device_code(&self) -> DeviceCodeRecord {
        let record = DeviceCodeRecord {
            device_auth_id: format!("device-auth-{}", short_token()),
            user_code: generate_user_code(),
            approved: false,
            polls: 0,
            authorization_code: format!("device-code-{}", short_token()),
            code_challenge: format!("challenge-{}", short_token()),
            code_verifier: format!("verifier-{}", short_token()),
        };
        let mut inner = self.inner.write().await;
        inner
            .device_codes
            .insert(record.device_auth_id.clone(), record.clone());
        record
    }

    pub async fn mark_device_code_approved(&self, user_code: &str) -> bool {
        let normalized = user_code.trim().to_uppercase();
        let mut inner = self.inner.write().await;
        for record in inner.device_codes.values_mut() {
            if record.user_code == normalized {
                record.approved = true;
                return true;
            }
        }
        false
    }

    pub async fn poll_device_token(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> DeviceTokenStatus {
        let normalized = user_code.trim().to_uppercase();
        let mut inner = self.inner.write().await;
        let Some(record) = inner.device_codes.get_mut(device_auth_id) else {
            return DeviceTokenStatus::Unknown;
        };
        if record.user_code != normalized {
            return DeviceTokenStatus::Unknown;
        }

        record.polls += 1;
        if !record.approved
            && self.args.device_code_auto_approve
            && record.polls > self.args.device_code_pending_polls
        {
            record.approved = true;
        }

        if !record.approved {
            return DeviceTokenStatus::Pending;
        }

        DeviceTokenStatus::Approved {
            authorization_code: record.authorization_code.clone(),
            code_challenge: record.code_challenge.clone(),
            code_verifier: record.code_verifier.clone(),
        }
    }

    pub async fn device_code_views(&self) -> Vec<DeviceCodeView> {
        let inner = self.inner.read().await;
        let mut records: Vec<DeviceCodeView> = inner
            .device_codes
            .values()
            .map(|record| DeviceCodeView {
                user_code: record.user_code.clone(),
                approved: record.approved,
                polls: record.polls,
            })
            .collect();
        records.sort_by(|left, right| left.user_code.cmp(&right.user_code));
        records
    }

    pub fn response_text_for_prompt(&self, prompt: &str) -> String {
        self.args
            .responses_output_text
            .replace("{prompt}", prompt)
            .replace("{model}", &self.args.model_slug)
            .replace("{account_id}", &self.args.chatgpt_account_id)
    }

    pub async fn create_task(&self, prompt: &str) -> TaskRecord {
        let now = unix_now_f64();
        let task = TaskRecord {
            task_id: format!("task_{}", short_token()),
            title: self.title_for_prompt(prompt).await,
            created_at: now,
            updated_at: now,
            current_turn_id: format!("turn_assistant_{}", short_short_token()),
            user_turn_id: format!("turn_user_{}", short_short_token()),
            assistant_turn_id: format!("turn_assistant_{}", short_short_token()),
            user_prompt: if prompt.trim().is_empty() {
                "Mock task prompt".to_string()
            } else {
                prompt.to_string()
            },
            assistant_response: self.response_text_for_prompt(if prompt.trim().is_empty() {
                "Mock task prompt"
            } else {
                prompt
            }),
            archived: false,
            has_unread_turn: false,
        };
        let mut inner = self.inner.write().await;
        inner.tasks.insert(task.task_id.clone(), task.clone());
        task
    }

    pub async fn list_tasks(&self) -> Vec<TaskRecord> {
        let inner = self.inner.read().await;
        let mut tasks: Vec<TaskRecord> = inner.tasks.values().cloned().collect();
        tasks.sort_by(|left, right| right.updated_at.total_cmp(&left.updated_at));
        tasks
    }

    pub async fn get_task(&self, task_id: &str) -> Option<TaskRecord> {
        let inner = self.inner.read().await;
        inner.tasks.get(task_id).cloned()
    }

    pub fn requirements_response(&self) -> RequirementsResponse {
        let digest = Sha256::digest(self.args.requirements_contents.as_bytes());
        RequirementsResponse {
            contents: self.args.requirements_contents.clone(),
            sha256: format!("{digest:x}"),
            updated_at: iso8601_now(),
            updated_by_user_id: self.args.chatgpt_user_id.clone(),
        }
    }

    async fn title_for_prompt(&self, prompt: &str) -> String {
        if !prompt.trim().is_empty() {
            return truncate_text(prompt, 72);
        }
        let inner = self.inner.read().await;
        format!("{} {}", self.args.task_title_prefix, inner.tasks.len() + 1)
    }

    fn auth_claims(&self) -> Value {
        json!({
            "chatgpt_plan_type": self.args.plan_type,
            "chatgpt_user_id": self.args.chatgpt_user_id,
            "chatgpt_account_id": self.args.chatgpt_account_id,
            "organization_id": self
                .args
                .organization_id
                .as_deref()
                .unwrap_or(self.args.chatgpt_account_id.as_str()),
            "project_id": self.args.project_id,
            "completed_platform_onboarding": self.args.completed_platform_onboarding,
            "is_org_owner": self.args.is_org_owner,
        })
    }
}

impl TaskRecord {
    pub fn as_task_response(&self) -> Value {
        json!({
            "id": self.task_id,
            "created_at": self.created_at,
            "title": self.title,
            "has_generated_title": true,
            "current_turn_id": self.current_turn_id,
            "has_unread_turn": self.has_unread_turn,
            "denormalized_metadata": Value::Null,
            "archived": self.archived,
            "external_pull_requests": [],
        })
    }

    pub fn as_list_item(&self) -> Value {
        json!({
            "id": self.task_id,
            "title": self.title,
            "has_generated_title": true,
            "updated_at": self.updated_at,
            "created_at": self.created_at,
            "task_status_display": {
                "status": "completed",
                "label": "Completed",
            },
            "archived": self.archived,
            "has_unread_turn": self.has_unread_turn,
            "pull_requests": [],
        })
    }

    pub fn as_turn_details(&self) -> Value {
        json!({
            "task": self.as_task_response(),
            "current_user_turn": {
                "id": self.user_turn_id,
                "attempt_placement": 0,
                "turn_status": "completed",
                "sibling_turn_ids": [],
                "input_items": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "content_type": "text",
                                "text": self.user_prompt,
                            }
                        ],
                    }
                ],
                "output_items": [],
                "worklog": { "messages": [] },
            },
            "current_assistant_turn": {
                "id": self.assistant_turn_id,
                "attempt_placement": 0,
                "turn_status": "completed",
                "sibling_turn_ids": [],
                "input_items": [],
                "output_items": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "content_type": "text",
                                "text": self.assistant_response,
                            }
                        ],
                    }
                ],
                "worklog": {
                    "messages": [
                        {
                            "author": { "role": "assistant" },
                            "content": {
                                "parts": [
                                    {
                                        "content_type": "text",
                                        "text": self.assistant_response,
                                    }
                                ]
                            },
                        }
                    ]
                },
            },
            "current_diff_task_turn": Value::Null,
        })
    }

    pub fn as_page_data(&self) -> TaskPageData {
        TaskPageData {
            task_id: self.task_id.clone(),
            title: self.title.clone(),
            user_prompt: self.user_prompt.clone(),
            assistant_response: self.assistant_response.clone(),
        }
    }
}

fn fake_jwt(payload: Value) -> String {
    let header = json!({ "alg": "none", "typ": "JWT" });
    [
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(payload.to_string()),
        URL_SAFE_NO_PAD.encode("signature"),
    ]
    .join(".")
}

pub fn iso8601_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn unix_now() -> i64 {
    Utc::now().timestamp()
}

fn unix_now_f64() -> f64 {
    Utc::now().timestamp_millis() as f64 / 1000.0
}

fn truncate_text(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= limit {
        compact
    } else {
        format!("{}...", compact[..limit.saturating_sub(3)].trim_end())
    }
}

fn short_token() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn short_short_token() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn generate_user_code() -> String {
    let code = Uuid::new_v4().simple().to_string();
    format!(
        "{}-{}",
        &code[..4].to_uppercase(),
        &code[4..8].to_uppercase()
    )
}
