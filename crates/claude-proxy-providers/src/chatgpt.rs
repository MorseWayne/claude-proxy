//! ChatGPT account provider adapter.
//!
//! Uses the same OpenAI Auth device flow and Codex Responses endpoint that
//! opencode uses for ChatGPT Pro/Plus authentication.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use claude_proxy_config::Settings;
use claude_proxy_core::*;
use futures::stream::BoxStream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::http::{apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response};
use crate::openai::{apply_openai_intent, log_request_observability, openai_model_info};
use crate::provider::{Provider, ProviderError};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEVICE_AUTHORIZE_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEFAULT_EXPIRES_IN: i64 = 3600;
const TOKEN_REFRESH_MARGIN_SECS: i64 = 60;
const MAX_DEVICE_POLL_ATTEMPTS: u32 = 60;
const DEFAULT_CHATGPT_INSTRUCTIONS: &str = "Follow the user's instructions. When calling tools, omit unused optional parameters instead of setting them to empty strings. If a tool call fails, retry with corrected arguments when possible instead of explaining the failed call to the user.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeviceCodeInfo {
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: Value,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

pub struct ChatGptAuth {
    token: RwLock<Option<ChatGptToken>>,
    token_dir: PathBuf,
    http_client: Client,
}

impl ChatGptAuth {
    pub async fn new(http_client: Client) -> Result<Arc<Self>, ProviderError> {
        let token_dir = Self::token_dir();
        fs::create_dir_all(&token_dir)
            .map_err(|e| ProviderError::Network(format!("failed to create token dir: {e}")))?;

        let auth = Arc::new(Self {
            token: RwLock::new(Self::load_token(&token_dir)),
            token_dir,
            http_client,
        });

        if auth.token.read().await.is_some() {
            info!("Loaded existing ChatGPT token from disk");
        }

        Ok(auth)
    }

    fn token_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("claude-proxy")
            .join("chatgpt")
    }

    fn token_path(dir: &Path) -> PathBuf {
        dir.join("token.json")
    }

    fn load_token(dir: &Path) -> Option<ChatGptToken> {
        let path = Self::token_path(dir);
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    fn save_token(&self, token: &ChatGptToken) {
        let path = Self::token_path(&self.token_dir);
        match serde_json::to_string_pretty(token) {
            Ok(body) => {
                if let Err(e) = fs::write(&path, body) {
                    warn!("Failed to save ChatGPT token: {e}");
                    return;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                }
            }
            Err(e) => warn!("Failed to serialize ChatGPT token: {e}"),
        }
    }

    pub async fn start_device_code(&self) -> Result<DeviceCodeInfo, ProviderError> {
        let response = self
            .http_client
            .post(DEVICE_USER_CODE_URL)
            .header("Content-Type", "application/json")
            .header("User-Agent", "opencode/claude-proxy")
            .json(&json!({ "client_id": CLIENT_ID }))
            .send()
            .await
            .map_err(|e| {
                ProviderError::Network(format!(
                    "ChatGPT device code request failed: {}",
                    fmt_reqwest_err(&e)
                ))
            })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let data: DeviceCodeResponse = response.json().await.map_err(|e| {
            ProviderError::Network(format!(
                "invalid ChatGPT device code response: {}",
                fmt_reqwest_err(&e)
            ))
        })?;

        Ok(DeviceCodeInfo {
            device_auth_id: data.device_auth_id,
            user_code: data.user_code,
            verification_uri: DEVICE_AUTHORIZE_URL.to_string(),
            interval: parse_interval(&data.interval).max(1),
        })
    }

    pub async fn complete_device_code(
        &self,
        info: &DeviceCodeInfo,
    ) -> Result<String, ProviderError> {
        for _ in 0..MAX_DEVICE_POLL_ATTEMPTS {
            let response = self
                .http_client
                .post(DEVICE_TOKEN_URL)
                .header("Content-Type", "application/json")
                .header("User-Agent", "opencode/claude-proxy")
                .json(&json!({
                    "device_auth_id": info.device_auth_id,
                    "user_code": info.user_code,
                }))
                .send()
                .await
                .map_err(|e| {
                    ProviderError::Network(format!(
                        "ChatGPT device token poll failed: {}",
                        fmt_reqwest_err(&e)
                    ))
                })?;

            if response.status().is_success() {
                let data: DeviceTokenResponse = response.json().await.map_err(|e| {
                    ProviderError::Network(format!(
                        "invalid ChatGPT device token response: {}",
                        fmt_reqwest_err(&e)
                    ))
                })?;
                let token = self
                    .exchange_authorization_code(
                        &data.authorization_code,
                        &data.code_verifier,
                        &format!("{ISSUER}/deviceauth/callback"),
                    )
                    .await?;
                let access = token.access_token.clone();
                self.store_token(token).await;
                return Ok(access);
            }

            let status = response.status().as_u16();
            if status != 403 && status != 404 {
                return Err(map_upstream_response(response).await);
            }

            sleep(Duration::from_secs(info.interval + 3)).await;
        }

        Err(ProviderError::Authentication(
            "ChatGPT authorization timed out, please try again".to_string(),
        ))
    }

    pub async fn run_device_flow(&self) -> Result<String, ProviderError> {
        let info = self.start_device_code().await?;

        eprintln!();
        eprintln!("+----------------------------------------------------------+");
        eprintln!("|  ChatGPT Authentication Required                         |");
        eprintln!("+----------------------------------------------------------+");
        eprintln!("|  Please visit: {}       |", info.verification_uri);
        eprintln!(
            "|  Enter code:   {}                                   |",
            info.user_code
        );
        eprintln!("+----------------------------------------------------------+");
        eprintln!();

        self.complete_device_code(&info).await
    }

    pub async fn get_token(&self) -> Result<ChatGptToken, ProviderError> {
        let needs_refresh = {
            let token = self.token.read().await;
            token.as_ref().is_none_or(|token| {
                chrono::Utc::now().timestamp() + TOKEN_REFRESH_MARGIN_SECS >= token.expires_at
            })
        };

        if !needs_refresh {
            return self
                .token
                .read()
                .await
                .clone()
                .ok_or_else(|| ProviderError::Authentication("no ChatGPT token".to_string()));
        }

        if self.token.read().await.is_none() {
            self.run_device_flow().await?;
        } else {
            self.refresh_access_token().await?;
        }

        self.token
            .read()
            .await
            .clone()
            .ok_or_else(|| ProviderError::Authentication("no ChatGPT token".to_string()))
    }

    async fn refresh_access_token(&self) -> Result<(), ProviderError> {
        let current =
            self.token.read().await.clone().ok_or_else(|| {
                ProviderError::Authentication("no ChatGPT refresh token".to_string())
            })?;

        let response = self
            .http_client
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", current.refresh_token.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .map_err(|e| {
                ProviderError::Network(format!(
                    "ChatGPT token refresh failed: {}",
                    fmt_reqwest_err(&e)
                ))
            })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let data: TokenResponse = response.json().await.map_err(|e| {
            ProviderError::Network(format!(
                "invalid ChatGPT token refresh response: {}",
                fmt_reqwest_err(&e)
            ))
        })?;
        let token = token_from_response(data, Some(&current.refresh_token));
        self.store_token(token).await;
        Ok(())
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<ChatGptToken, ProviderError> {
        let response = self
            .http_client
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", CLIENT_ID),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .map_err(|e| {
                ProviderError::Network(format!(
                    "ChatGPT token exchange failed: {}",
                    fmt_reqwest_err(&e)
                ))
            })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let data: TokenResponse = response.json().await.map_err(|e| {
            ProviderError::Network(format!(
                "invalid ChatGPT token exchange response: {}",
                fmt_reqwest_err(&e)
            ))
        })?;
        Ok(token_from_response(data, None))
    }

    async fn store_token(&self, token: ChatGptToken) {
        self.save_token(&token);
        let mut current = self.token.write().await;
        *current = Some(token);
    }
}

pub struct ChatGptProvider {
    id: String,
    http_client: Client,
    endpoint: String,
    auth: Arc<ChatGptAuth>,
}

impl ChatGptProvider {
    pub async fn new(
        id: &str,
        base_url: &str,
        proxy: &str,
        settings: &Settings,
    ) -> Result<Self, ProviderError> {
        let http_client = build_http_client(proxy, settings)?;
        let auth = ChatGptAuth::new(http_client.clone()).await?;

        Ok(Self {
            id: id.to_string(),
            http_client,
            endpoint: codex_responses_endpoint(base_url),
            auth,
        })
    }
}

#[async_trait]
impl Provider for ChatGptProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let token = self.auth.get_token().await?;
        let request = apply_openai_intent(request);
        let body = build_chatgpt_responses_body(&request);
        log_request_observability("chatgpt", "/responses", &body);

        let mut request_builder = self
            .http_client
            .post(&self.endpoint)
            .bearer_auth(&token.access_token)
            .header("Content-Type", "application/json")
            .header("originator", "opencode")
            .header("User-Agent", "opencode/claude-proxy");

        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }

        let response = request_builder.json(&body).send().await.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Network(fmt_reqwest_err(&e))
            }
        })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        Ok(crate::copilot::responses::stream_responses_response(
            response,
        ))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(chatgpt_models())
    }
}

fn build_http_client(proxy: &str, settings: &Settings) -> Result<Client, ProviderError> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(settings.http.connect_timeout))
        .read_timeout(Duration::from_secs(settings.http.read_timeout));

    if !proxy.is_empty() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|e| ProviderError::Network(format!("invalid proxy: {e}")))?,
        );
    }

    builder = apply_extra_ca_certs(builder, &settings.http.extra_ca_certs)?;

    builder.build().map_err(|e| {
        ProviderError::Network(format!(
            "failed to build HTTP client: {}",
            fmt_reqwest_err(&e)
        ))
    })
}

fn codex_responses_endpoint(base_url: &str) -> String {
    let base = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url.trim_end_matches('/')
    };

    if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{base}/responses")
    }
}

fn build_chatgpt_responses_body(request: &MessagesRequest) -> Value {
    let mut body = crate::copilot::responses::convert_to_responses(request);
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
        object.insert("stream".to_string(), json!(true));
        let missing_instructions = object
            .get("instructions")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if missing_instructions {
            object.insert(
                "instructions".to_string(),
                json!(DEFAULT_CHATGPT_INSTRUCTIONS),
            );
        }
    }
    body
}

fn chatgpt_models() -> Vec<ModelInfo> {
    [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
        "gpt-5.3-codex-spark",
        "gpt-5.2",
    ]
    .into_iter()
    .map(openai_model_info)
    .collect()
}

fn token_from_response(data: TokenResponse, fallback_refresh: Option<&str>) -> ChatGptToken {
    let account_id = extract_account_id(&data);
    let refresh_token = data
        .refresh_token
        .clone()
        .or_else(|| fallback_refresh.map(str::to_string))
        .unwrap_or_default();

    ChatGptToken {
        access_token: data.access_token,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp() + data.expires_in.unwrap_or(DEFAULT_EXPIRES_IN),
        account_id,
    }
}

fn parse_interval(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(5)
}

fn extract_account_id(tokens: &TokenResponse) -> Option<String> {
    tokens
        .id_token
        .as_deref()
        .and_then(extract_account_id_from_jwt)
        .or_else(|| extract_account_id_from_jwt(&tokens.access_token))
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    extract_account_id_from_claims(&claims)
}

fn extract_account_id_from_claims(claims: &Value) -> Option<String> {
    claims["chatgpt_account_id"]
        .as_str()
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth["chatgpt_account_id"].as_str())
        })
        .or_else(|| {
            claims["organizations"]
                .as_array()
                .and_then(|orgs| orgs.first())
                .and_then(|org| org["id"].as_str())
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let body = URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn extracts_account_id_from_nested_claims() {
        let token = jwt(json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-nested"
            }
        }));

        assert_eq!(
            extract_account_id_from_jwt(&token).as_deref(),
            Some("acc-nested")
        );
    }

    #[test]
    fn extracts_account_id_from_organizations_fallback() {
        let token = jwt(json!({
            "organizations": [{"id": "org-123"}]
        }));

        assert_eq!(
            extract_account_id_from_jwt(&token).as_deref(),
            Some("org-123")
        );
    }

    #[test]
    fn builds_default_codex_responses_endpoint() {
        assert_eq!(
            codex_responses_endpoint(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_endpoint("https://example.test/base"),
            "https://example.test/base/responses"
        );
        assert_eq!(
            codex_responses_endpoint("https://example.test/base/responses"),
            "https://example.test/base/responses"
        );
    }

    #[test]
    fn chatgpt_models_include_reasoning_capabilities() {
        let models = chatgpt_models();
        let gpt55 = models
            .iter()
            .find(|model| model.model_id == "gpt-5.5")
            .expect("gpt-5.5 model");

        assert_eq!(gpt55.max_output_tokens, Some(128_000));
        assert!(
            gpt55
                .supported_endpoints
                .contains(&"/responses".to_string())
        );
        assert_eq!(
            gpt55.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn chatgpt_responses_body_adds_default_instructions() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["instructions"], DEFAULT_CHATGPT_INSTRUCTIONS);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn chatgpt_responses_body_preserves_system_instructions() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: Some(SystemPrompt::Text("Use terse answers.".to_string())),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["instructions"], "Use terse answers.");
    }

    #[test]
    fn chatgpt_intent_fast_affects_responses_body() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({"intent": "fast"})),
            extra: Default::default(),
        };

        let req = apply_openai_intent(req);
        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["instructions"], DEFAULT_CHATGPT_INSTRUCTIONS);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert!(body["reasoning"].get("summary").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }
}
