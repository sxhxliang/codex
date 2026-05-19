use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use reqwest::header::ACCEPT;
use reqwest::header::AUTHORIZATION;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use serde::Serialize;
use url::Url;

const OAUTH_USER_AGENT: &str = "mock-chatgpt-account-server/1.0";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoginUiConfig {
    social_providers: Vec<OAuthSocialLoginProvider>,
}

impl LoginUiConfig {
    pub fn social_login_providers(&self) -> Vec<SocialLoginProvider> {
        self.social_providers
            .iter()
            .map(OAuthSocialLoginProvider::as_view)
            .collect()
    }

    pub fn social_login_provider(&self, provider_id: &str) -> Option<&OAuthSocialLoginProvider> {
        self.social_providers
            .iter()
            .find(|provider| provider.id == provider_id)
    }

    fn from_toml_str(raw: &str) -> Result<Self> {
        let parsed: LoginUiConfigFile = toml::from_str(raw)?;
        let social_providers = [
            build_social_login_provider(
                "google",
                "Continue with Google",
                parsed.social_login.google,
            )?,
            build_social_login_provider(
                "github",
                "Continue with GitHub",
                parsed.social_login.github,
            )?,
        ]
        .into_iter()
        .flatten()
        .collect();

        Ok(Self { social_providers })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocialLoginProvider {
    pub id: String,
    pub label: String,
    pub subtitle: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthSocialLoginProvider {
    pub id: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    pub authorize_url: String,
    pub token_url: String,
    pub user_info_url: String,
    pub user_email_url: Option<String>,
    pub scopes: Vec<String>,
}

impl OAuthSocialLoginProvider {
    pub fn authorize_url(&self, redirect_uri: &str, oauth_state: &str) -> Result<String> {
        let mut url = Url::parse(&self.authorize_url)
            .with_context(|| format!("invalid authorize_url for {}", self.id))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &self.client_id);
            query.append_pair("redirect_uri", redirect_uri);
            query.append_pair("scope", &self.scopes.join(" "));
            query.append_pair("state", oauth_state);
        }
        Ok(url.into())
    }

    pub async fn exchange_code_for_identity(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<SocialLoginIdentity> {
        let client = reqwest::Client::new();
        let token_response = client
            .post(&self.token_url)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, OAUTH_USER_AGENT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
            ])
            .send()
            .await
            .with_context(|| format!("failed to exchange {} authorization code", self.id))?
            .error_for_status()
            .with_context(|| format!("{} token endpoint rejected the authorization code", self.id))?
            .json::<OAuthAccessTokenResponse>()
            .await
            .with_context(|| format!("failed to parse {} token response", self.id))?;

        let user_info = client
            .get(&self.user_info_url)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", token_response.access_token),
            )
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, OAUTH_USER_AGENT)
            .send()
            .await
            .with_context(|| format!("failed to fetch {} user profile", self.id))?
            .error_for_status()
            .with_context(|| {
                format!(
                    "{} user profile endpoint rejected the access token",
                    self.id
                )
            })?
            .json::<OAuthUserInfo>()
            .await
            .with_context(|| format!("failed to parse {} user profile response", self.id))?;

        let session_username = match trim_to_option(user_info.email) {
            Some(email) => email,
            None => {
                let email = if let Some(user_email_url) = &self.user_email_url {
                    resolve_user_email(&client, user_email_url, &token_response.access_token)
                        .await?
                } else {
                    None
                };
                email
                    .or_else(|| trim_to_option(user_info.login))
                    .or_else(|| trim_to_option(user_info.name))
                    .unwrap_or_else(|| self.label.clone())
            }
        };

        Ok(SocialLoginIdentity { session_username })
    }

    fn as_view(&self) -> SocialLoginProvider {
        SocialLoginProvider {
            id: self.id.clone(),
            label: self.label.clone(),
            subtitle: self.subtitle.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocialLoginIdentity {
    pub session_username: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LoginUiConfigFile {
    social_login: SocialLoginProvidersFile,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct SocialLoginProvidersFile {
    google: Option<ConfiguredSocialLoginProvider>,
    github: Option<ConfiguredSocialLoginProvider>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct ConfiguredSocialLoginProvider {
    enabled: bool,
    label: Option<String>,
    subtitle: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
    user_info_url: Option<String>,
    user_email_url: Option<String>,
    scopes: Option<Vec<String>>,
}

impl Default for ConfiguredSocialLoginProvider {
    fn default() -> Self {
        Self {
            enabled: true,
            label: None,
            subtitle: None,
            client_id: None,
            client_secret: None,
            authorize_url: None,
            token_url: None,
            user_info_url: None,
            user_email_url: None,
            scopes: None,
        }
    }
}

#[derive(Clone, Copy)]
struct SocialLoginProviderDefaults {
    authorize_url: &'static str,
    token_url: &'static str,
    user_info_url: &'static str,
    user_email_url: Option<&'static str>,
    scopes: &'static [&'static str],
}

#[derive(Debug, Deserialize)]
struct OAuthAccessTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct OAuthUserInfo {
    email: Option<String>,
    login: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthUserEmailRecord {
    email: String,
    primary: Option<bool>,
    verified: Option<bool>,
}

pub fn load_login_ui_config(config_path: Option<&Path>) -> Result<LoginUiConfig> {
    let Some(config_path) = config_path else {
        return Ok(LoginUiConfig::default());
    };

    let raw = fs::read_to_string(config_path).with_context(|| {
        format!(
            "failed to read social login config {}",
            config_path.display()
        )
    })?;
    LoginUiConfig::from_toml_str(&raw).with_context(|| {
        format!(
            "failed to parse social login config {}",
            config_path.display()
        )
    })
}

async fn resolve_user_email(
    client: &reqwest::Client,
    user_email_url: &str,
    access_token: &str,
) -> Result<Option<String>> {
    let email_records = client
        .get(user_email_url)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, OAUTH_USER_AGENT)
        .send()
        .await
        .context("failed to fetch provider email list")?
        .error_for_status()
        .context("provider email endpoint rejected the access token")?
        .json::<Vec<OAuthUserEmailRecord>>()
        .await
        .context("failed to parse provider email response")?;

    let email = email_records
        .iter()
        .find(|record| record.primary.unwrap_or(false) && record.verified.unwrap_or(true))
        .or_else(|| {
            email_records
                .iter()
                .find(|record| record.verified.unwrap_or(true))
        })
        .or_else(|| email_records.first())
        .map(|record| record.email.trim().to_string());

    Ok(email.filter(|email| !email.is_empty()))
}

fn build_social_login_provider(
    provider_id: &str,
    default_label: &str,
    configured: Option<ConfiguredSocialLoginProvider>,
) -> Result<Option<OAuthSocialLoginProvider>> {
    let Some(configured) = configured else {
        return Ok(None);
    };
    if !configured.enabled {
        return Ok(None);
    }

    let defaults = social_login_provider_defaults(provider_id)?;
    let label = trim_to_option(configured.label).unwrap_or_else(|| default_label.to_string());
    let client_id = required_trimmed_string(configured.client_id, provider_id, "client_id")?;
    let client_secret =
        required_trimmed_string(configured.client_secret, provider_id, "client_secret")?;
    let authorize_url = validate_url(
        trim_to_option(configured.authorize_url)
            .unwrap_or_else(|| defaults.authorize_url.to_string()),
        provider_id,
        "authorize_url",
    )?;
    let token_url = validate_url(
        trim_to_option(configured.token_url).unwrap_or_else(|| defaults.token_url.to_string()),
        provider_id,
        "token_url",
    )?;
    let user_info_url = validate_url(
        trim_to_option(configured.user_info_url)
            .unwrap_or_else(|| defaults.user_info_url.to_string()),
        provider_id,
        "user_info_url",
    )?;
    let user_email_url = match trim_to_option(configured.user_email_url) {
        Some(user_email_url) => Some(validate_url(user_email_url, provider_id, "user_email_url")?),
        None => defaults
            .user_email_url
            .map(std::string::ToString::to_string),
    };
    let scopes = configured
        .scopes
        .unwrap_or_else(|| {
            defaults
                .scopes
                .iter()
                .map(|scope| (*scope).to_string())
                .collect()
        })
        .into_iter()
        .filter_map(|scope| trim_to_option(Some(scope)))
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        bail!("social login provider {provider_id} must configure at least one scope");
    }

    Ok(Some(OAuthSocialLoginProvider {
        id: provider_id.to_string(),
        label,
        subtitle: trim_to_option(configured.subtitle),
        client_id,
        client_secret,
        authorize_url,
        token_url,
        user_info_url,
        user_email_url,
        scopes,
    }))
}

fn social_login_provider_defaults(provider_id: &str) -> Result<SocialLoginProviderDefaults> {
    match provider_id {
        "google" => Ok(SocialLoginProviderDefaults {
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url: "https://oauth2.googleapis.com/token",
            user_info_url: "https://openidconnect.googleapis.com/v1/userinfo",
            user_email_url: None,
            scopes: &["openid", "email", "profile"],
        }),
        "github" => Ok(SocialLoginProviderDefaults {
            authorize_url: "https://github.com/login/oauth/authorize",
            token_url: "https://github.com/login/oauth/access_token",
            user_info_url: "https://api.github.com/user",
            user_email_url: Some("https://api.github.com/user/emails"),
            scopes: &["read:user", "user:email"],
        }),
        _ => bail!("unsupported social login provider {provider_id}"),
    }
}

fn required_trimmed_string(
    value: Option<String>,
    provider_id: &str,
    field_name: &str,
) -> Result<String> {
    trim_to_option(value).with_context(|| {
        format!("social login provider {provider_id} requires a non-empty {field_name}")
    })
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

fn validate_url(value: String, provider_id: &str, field_name: &str) -> Result<String> {
    Url::parse(&value).with_context(|| {
        format!("social login provider {provider_id} has invalid {field_name}: {value}")
    })?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_enabled_social_login_providers() {
        let config = LoginUiConfig::from_toml_str(
            r#"
[social_login.google]
subtitle = "debug@example.com"
client_id = "google-client"
client_secret = "google-secret"

[social_login.github]
label = "Use GitHub SSO"
subtitle = "@debug-codex"
client_id = "github-client"
client_secret = "github-secret"
"#,
        )
        .expect("social login config");

        assert_eq!(
            config.social_login_providers(),
            vec![
                SocialLoginProvider {
                    id: "google".to_string(),
                    label: "Continue with Google".to_string(),
                    subtitle: Some("debug@example.com".to_string()),
                },
                SocialLoginProvider {
                    id: "github".to_string(),
                    label: "Use GitHub SSO".to_string(),
                    subtitle: Some("@debug-codex".to_string()),
                },
            ]
        );
        assert_eq!(
            config
                .social_login_provider("google")
                .expect("google provider")
                .client_id,
            "google-client"
        );
        assert_eq!(
            config
                .social_login_provider("github")
                .expect("github provider")
                .client_secret,
            "github-secret"
        );
    }

    #[test]
    fn rejects_enabled_provider_without_client_secret() {
        let error = LoginUiConfig::from_toml_str(
            r#"
[social_login.google]
client_id = "google-client"
"#,
        )
        .expect_err("missing client_secret should fail");

        assert!(
            error
                .to_string()
                .contains("social login provider google requires a non-empty client_secret")
        );
    }

    #[test]
    fn skips_disabled_social_login_provider() {
        let config = LoginUiConfig::from_toml_str(
            r#"
[social_login.google]
enabled = false

[social_login.github]
subtitle = "@debug-codex"
client_id = "github-client"
client_secret = "github-secret"
"#,
        )
        .expect("social login config");

        assert_eq!(
            config.social_login_providers(),
            vec![SocialLoginProvider {
                id: "github".to_string(),
                label: "Continue with GitHub".to_string(),
                subtitle: Some("@debug-codex".to_string()),
            }]
        );
    }
}
