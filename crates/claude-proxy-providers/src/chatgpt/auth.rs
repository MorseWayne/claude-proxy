use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::http::{fmt_reqwest_err, map_upstream_response, read_upstream_response_json};
use crate::provider::ProviderError;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const DEVICE_AUTHORIZE_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEFAULT_EXPIRES_IN: i64 = 3600;
const TOKEN_REFRESH_MARGIN_SECS: i64 = 60;
const MAX_DEVICE_POLL_ATTEMPTS: u32 = 60;

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
    token_refresh_lock: Mutex<()>,
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
            token_refresh_lock: Mutex::new(()),
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
                if let Err(e) = write_token_file(&path, &body) {
                    warn!("Failed to save ChatGPT token: {e}");
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

        let data: DeviceCodeResponse =
            read_upstream_response_json(response, "invalid ChatGPT device code response").await?;

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
                let data: DeviceTokenResponse =
                    read_upstream_response_json(response, "invalid ChatGPT device token response")
                        .await?;
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
        if !self.token_needs_refresh().await {
            return self.current_token().await;
        }

        let _refresh_guard = self.token_refresh_lock.lock().await;

        if !self.token_needs_refresh().await {
            return self.current_token().await;
        }

        if self.token.read().await.is_none() {
            self.run_device_flow().await?;
        } else {
            self.refresh_access_token().await?;
        }

        self.current_token().await
    }

    pub async fn get_existing_token(&self) -> Result<ChatGptToken, ProviderError> {
        self.current_token().await?;
        if !self.token_needs_refresh().await {
            return self.current_token().await;
        }

        let _refresh_guard = self.token_refresh_lock.lock().await;
        if self.token.read().await.is_none() {
            return Err(ProviderError::Authentication(
                "no ChatGPT token".to_string(),
            ));
        }
        if self.token_needs_refresh().await {
            self.refresh_access_token().await?;
        }
        self.current_token().await
    }

    pub async fn force_refresh_token(&self) -> Result<ChatGptToken, ProviderError> {
        let _refresh_guard = self.token_refresh_lock.lock().await;
        self.refresh_access_token().await?;
        self.current_token().await
    }

    pub async fn clear_token(&self) {
        let path = Self::token_path(&self.token_dir);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("Failed to remove ChatGPT token: {e}");
        }
        let mut current = self.token.write().await;
        *current = None;
    }

    async fn token_needs_refresh(&self) -> bool {
        let token = self.token.read().await;
        token.as_ref().is_none_or(|token| {
            chrono::Utc::now().timestamp() + TOKEN_REFRESH_MARGIN_SECS >= token.expires_at
        })
    }

    async fn current_token(&self) -> Result<ChatGptToken, ProviderError> {
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

        let data: TokenResponse =
            read_upstream_response_json(response, "invalid ChatGPT token refresh response").await?;
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

        let data: TokenResponse =
            read_upstream_response_json(response, "invalid ChatGPT token exchange response")
                .await?;
        Ok(token_from_response(data, None))
    }

    async fn store_token(&self, token: ChatGptToken) {
        self.save_token(&token);
        let mut current = self.token.write().await;
        *current = Some(token);
    }
}

fn write_token_file(path: &Path, body: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(body.as_bytes())
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

    fn auth_with_token(token: Option<ChatGptToken>) -> ChatGptAuth {
        auth_with_token_dir(token, PathBuf::new())
    }

    fn auth_with_token_dir(token: Option<ChatGptToken>, token_dir: PathBuf) -> ChatGptAuth {
        ChatGptAuth {
            token: RwLock::new(token),
            token_refresh_lock: Mutex::new(()),
            token_dir,
            http_client: Client::new(),
        }
    }

    fn jwt(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let body = URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("{header}.{body}.sig")
    }

    #[tokio::test]
    async fn token_refresh_state_uses_margin() {
        let now = chrono::Utc::now().timestamp();
        let fresh = auth_with_token(Some(ChatGptToken {
            access_token: "fresh".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: now + TOKEN_REFRESH_MARGIN_SECS + 30,
            account_id: None,
        }));
        let stale = auth_with_token(Some(ChatGptToken {
            access_token: "stale".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: now + TOKEN_REFRESH_MARGIN_SECS - 1,
            account_id: None,
        }));
        let missing = auth_with_token(None);

        assert!(!fresh.token_needs_refresh().await);
        assert!(stale.token_needs_refresh().await);
        assert!(missing.token_needs_refresh().await);
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

    #[cfg(unix)]
    #[test]
    fn save_token_restricts_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let token_dir = std::env::temp_dir().join(format!(
            "claude-proxy-chatgpt-auth-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&token_dir).unwrap();
        let path = ChatGptAuth::token_path(&token_dir);
        fs::write(&path, "old token").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let auth = auth_with_token_dir(None, token_dir.clone());
        auth.save_token(&ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: 123,
            account_id: Some("account".to_string()),
        });

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(fs::read_to_string(&path).unwrap().contains("access"));

        fs::remove_dir_all(token_dir).unwrap();
    }
}
