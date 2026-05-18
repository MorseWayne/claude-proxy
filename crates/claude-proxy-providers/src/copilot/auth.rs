use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, sleep};
use tracing::{error, info, warn};

use crate::http::fmt_reqwest_err;
use crate::provider::ProviderError;

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_SCOPES: &str = "read:user";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

const POLL_INTERVAL_SECS: u64 = 5;
const MAX_POLL_ATTEMPTS: u32 = 60;
const DEVICE_CODE_REQUEST_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotToken {
    pub token: String,
    pub expires_at: i64,
    pub refresh_in: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
}

/// Public device code info returned to callers for display.
#[derive(Debug, Clone)]
pub struct DeviceCodeInfo {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AccessTokenResponse {
    Success {
        access_token: String,
        #[allow(dead_code)]
        token_type: String,
        #[allow(dead_code)]
        scope: String,
    },
    Error {
        error: String,
        #[serde(rename = "error_description")]
        #[allow(dead_code)]
        error_description: Option<String>,
    },
}

/// Manages GitHub OAuth Device Flow and Copilot token lifecycle.
pub struct CopilotAuth {
    github_token: RwLock<Option<String>>,
    copilot_token: RwLock<Option<CopilotToken>>,
    token_refresh_lock: Mutex<()>,
    token_dir: PathBuf,
    http_client: Client,
    oauth_app: String,
}

impl CopilotAuth {
    /// Create a new auth manager.  Loads persisted GitHub token if available.
    pub async fn new(http_client: Client, oauth_app: &str) -> Result<Arc<Self>, ProviderError> {
        let token_dir = Self::token_dir();
        fs::create_dir_all(&token_dir)
            .map_err(|e| ProviderError::Network(format!("failed to create token dir: {e}")))?;

        let github_token = Self::load_github_token(&token_dir);

        let auth = Arc::new(Self {
            github_token: RwLock::new(github_token),
            copilot_token: RwLock::new(None),
            token_refresh_lock: Mutex::new(()),
            token_dir,
            http_client,
            oauth_app: oauth_app.to_string(),
        });

        if auth.github_token.read().await.is_some() {
            info!("Loaded existing GitHub token from disk");
            match auth.refresh_copilot_token().await {
                Ok(_) => info!("Copilot token refreshed"),
                Err(e) => warn!("Failed initial Copilot token refresh: {e}"),
            }
        }

        Ok(auth)
    }

    fn token_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("claude-proxy")
            .join("copilot")
    }

    fn github_token_path(dir: &Path) -> PathBuf {
        dir.join("github_token")
    }

    fn load_github_token(dir: &Path) -> Option<String> {
        let path = Self::github_token_path(dir);
        fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn save_github_token(&self, token: &str) {
        let path = Self::github_token_path(&self.token_dir);
        if let Err(e) = fs::write(&path, token) {
            error!("Failed to save GitHub token: {e}");
        }
        // Set restrictive permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }

    /// Check if we have a valid GitHub token.
    pub async fn has_github_token(&self) -> bool {
        self.github_token.read().await.is_some()
    }

    async fn reload_github_token_from_disk(&self) -> bool {
        let Some(token) = Self::load_github_token(&self.token_dir) else {
            return false;
        };

        let mut current = self.github_token.write().await;
        if current.as_deref() == Some(token.as_str()) {
            return true;
        }

        *current = Some(token);
        info!("Reloaded GitHub token from disk");
        true
    }

    /// Request a device code from GitHub and return info for display.
    /// Caller should show the URL + user_code to the user, then call
    /// `complete_device_code` to poll for completion.
    pub async fn start_device_code(&self) -> Result<DeviceCodeInfo, ProviderError> {
        let resp = self.request_device_code().await?;
        Ok(DeviceCodeInfo {
            device_code: resp.device_code,
            user_code: resp.user_code,
            verification_uri: resp.verification_uri,
            interval: resp.interval,
        })
    }

    /// Poll GitHub until the user authorizes the device code.
    /// Returns the GitHub access token on success.
    pub async fn complete_device_code(
        &self,
        info: &DeviceCodeInfo,
    ) -> Result<String, ProviderError> {
        let dc = DeviceCodeResponse {
            device_code: info.device_code.clone(),
            user_code: info.user_code.clone(),
            verification_uri: info.verification_uri.clone(),
            interval: info.interval,
        };
        self.poll_for_token(&dc).await
    }

    /// Run the GitHub OAuth Device Flow interactively.
    /// Returns the GitHub access token.
    pub async fn run_device_flow(&self) -> Result<String, ProviderError> {
        info!("Starting GitHub OAuth device flow...");

        let device_code = self.request_device_code().await?;

        eprintln!();
        eprintln!("╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  Copilot Authentication Required                         ║");
        eprintln!("╠══════════════════════════════════════════════════════════╣");
        eprintln!("║  Please visit: {}  ║", device_code.verification_uri);
        eprintln!(
            "║  Enter code:   {}                                  ║",
            device_code.user_code
        );
        eprintln!("╚══════════════════════════════════════════════════════════╝");
        eprintln!();

        self.poll_for_token(&device_code).await
    }

    async fn request_device_code(&self) -> Result<DeviceCodeResponse, ProviderError> {
        let client = &self.http_client;
        for attempt in 1..=DEVICE_CODE_REQUEST_ATTEMPTS {
            let resp = client
                .post(GITHUB_DEVICE_CODE_URL)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "client_id": GITHUB_CLIENT_ID,
                    "scope": GITHUB_SCOPES,
                }))
                .send()
                .await;

            let resp = match resp {
                Ok(resp) => resp,
                Err(e) => {
                    let err = fmt_reqwest_err(&e);
                    if attempt == DEVICE_CODE_REQUEST_ATTEMPTS {
                        return Err(ProviderError::Network(format!(
                            "device code request failed after {attempt} attempts: {err}"
                        )));
                    }
                    warn!(
                        "device code request failed (attempt {attempt}/{DEVICE_CODE_REQUEST_ATTEMPTS}): {err}; retrying"
                    );
                    sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                    continue;
                }
            };

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Authentication(format!(
                    "device code request rejected: {body}"
                )));
            }

            return resp.json::<DeviceCodeResponse>().await.map_err(|e| {
                ProviderError::Network(format!(
                    "invalid device code response: {}",
                    fmt_reqwest_err(&e)
                ))
            });
        }

        unreachable!("device code request loop must return");
    }

    async fn poll_for_token(&self, dc: &DeviceCodeResponse) -> Result<String, ProviderError> {
        let client = &self.http_client;
        let poll_interval = Duration::from_secs(dc.interval.max(POLL_INTERVAL_SECS));

        for attempt in 0..MAX_POLL_ATTEMPTS {
            sleep(poll_interval).await;

            let resp = client
                .post(GITHUB_ACCESS_TOKEN_URL)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "client_id": GITHUB_CLIENT_ID,
                    "device_code": dc.device_code,
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                }))
                .send()
                .await
                .map_err(|e| {
                    ProviderError::Network(format!("token poll failed: {}", fmt_reqwest_err(&e)))
                })?;

            let body_text = resp.text().await.unwrap_or_default();
            let parsed: AccessTokenResponse = serde_json::from_str(&body_text)
                .map_err(|e| ProviderError::Network(format!("invalid token response: {e}")))?;

            match parsed {
                AccessTokenResponse::Success { access_token, .. } => {
                    let mut token = self.github_token.write().await;
                    *token = Some(access_token.clone());
                    self.save_github_token(&access_token);
                    info!("GitHub OAuth device flow completed successfully");
                    return Ok(access_token);
                }
                AccessTokenResponse::Error { error, .. } => {
                    if error == "authorization_pending" {
                        info!(
                            "Waiting for authorization... (attempt {}/{})",
                            attempt + 1,
                            MAX_POLL_ATTEMPTS
                        );
                        continue;
                    }
                    if error == "slow_down" {
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                    if error == "expired_token" {
                        return Err(ProviderError::Authentication(
                            "device code expired, please restart authentication".to_string(),
                        ));
                    }
                    return Err(ProviderError::Authentication(format!(
                        "authorization failed: {error}"
                    )));
                }
            }
        }

        Err(ProviderError::Authentication(
            "authorization timed out, please try again".to_string(),
        ))
    }

    /// Exchange the GitHub token for a Copilot token.
    pub async fn refresh_copilot_token(&self) -> Result<(), ProviderError> {
        let _refresh_guard = self.token_refresh_lock.lock().await;
        self.refresh_copilot_token_inner().await
    }

    async fn refresh_copilot_token_inner(&self) -> Result<(), ProviderError> {
        let github_token = self
            .github_token
            .read()
            .await
            .clone()
            .ok_or_else(|| ProviderError::Authentication("no GitHub token".to_string()))?;

        if self.oauth_app == "opencode" {
            let expiry = chrono::Utc::now().timestamp() + 3600;
            let mut token = self.copilot_token.write().await;
            *token = Some(CopilotToken {
                token: github_token,
                expires_at: expiry,
                refresh_in: 1800,
            });
            return Ok(());
        }

        let client = &self.http_client;
        let resp = client
            .get(COPILOT_TOKEN_URL)
            .header("Authorization", format!("token {github_token}"))
            .header("Accept", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.46.0")
            .header("editor-version", "vscode/1.118.0")
            .send()
            .await
            .map_err(|e| {
                ProviderError::Network(format!(
                    "copilot token request failed: {}",
                    fmt_reqwest_err(&e)
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();

            if status == 401 {
                let mut token = self.github_token.write().await;
                *token = None;
                let path = Self::github_token_path(&self.token_dir);
                let _ = fs::remove_file(&path);
                return Err(ProviderError::Authentication(
                    "GitHub token expired, please re-authenticate".to_string(),
                ));
            }

            return Err(ProviderError::UpstreamError { status, body });
        }

        let data: Value = resp.json().await.map_err(|e| {
            ProviderError::Network(format!(
                "invalid copilot token response: {}",
                fmt_reqwest_err(&e)
            ))
        })?;

        let token_str = data["token"]
            .as_str()
            .ok_or_else(|| {
                ProviderError::Network("copilot token missing 'token' field".to_string())
            })?
            .to_string();
        let expires_at = data["expires_at"].as_i64().unwrap_or(0);
        let refresh_in = data["refresh_in"].as_i64().unwrap_or(1500);

        let mut token = self.copilot_token.write().await;
        *token = Some(CopilotToken {
            token: token_str,
            expires_at,
            refresh_in,
        });

        info!(
            "Copilot token refreshed (expires in {}s, refresh in {}s)",
            expires_at - chrono::Utc::now().timestamp(),
            refresh_in
        );

        Ok(())
    }

    /// Get a valid Copilot token, refreshing if needed.
    pub async fn get_token(&self) -> Result<String, ProviderError> {
        if !self.has_github_token().await {
            self.reload_github_token_from_disk().await;
        }

        if self.has_github_token().await && !self.copilot_token_needs_refresh().await {
            return self.current_copilot_token().await;
        }

        let _refresh_guard = self.token_refresh_lock.lock().await;

        if !self.has_github_token().await {
            self.reload_github_token_from_disk().await;
        }

        if !self.has_github_token().await {
            let _token = self.run_device_flow().await?;
            self.refresh_copilot_token_inner().await?;
            return self.current_copilot_token().await;
        }

        if self.copilot_token_needs_refresh().await
            && let Err(e) = self.refresh_copilot_token_inner().await
        {
            warn!("Failed to refresh Copilot token: {e}");
            if let Some(token) = self.current_unexpired_copilot_token().await {
                return Ok(token);
            }
            return Err(e);
        }

        self.current_copilot_token().await
    }

    async fn copilot_token_needs_refresh(&self) -> bool {
        let ct = self.copilot_token.read().await;
        ct.as_ref().is_none_or(|t| {
            let now = chrono::Utc::now().timestamp();
            now + t.refresh_in >= t.expires_at
        })
    }

    async fn current_unexpired_copilot_token(&self) -> Option<String> {
        let ct = self.copilot_token.read().await;
        ct.as_ref().and_then(|t| {
            let now = chrono::Utc::now().timestamp();
            (now < t.expires_at).then(|| t.token.clone())
        })
    }

    async fn current_copilot_token(&self) -> Result<String, ProviderError> {
        self.copilot_token
            .read()
            .await
            .as_ref()
            .map(|t| t.token.clone())
            .ok_or_else(|| ProviderError::Authentication("no copilot token".to_string()))
    }

    /// Start a background token refresh loop.
    #[allow(dead_code)]
    pub fn start_refresh_loop(self: &Arc<Self>) {
        let auth = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                let needs_refresh = {
                    let ct = auth.copilot_token.read().await;
                    ct.as_ref().is_none_or(|t| {
                        let now = chrono::Utc::now().timestamp();
                        now + t.refresh_in - 60 >= t.expires_at
                    })
                };
                if needs_refresh && let Err(e) = auth.refresh_copilot_token().await {
                    warn!("Background Copilot token refresh failed: {e}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with_token(token: Option<CopilotToken>) -> CopilotAuth {
        CopilotAuth {
            github_token: RwLock::new(Some("github-token".to_string())),
            copilot_token: RwLock::new(token),
            token_refresh_lock: Mutex::new(()),
            token_dir: PathBuf::new(),
            http_client: Client::new(),
            oauth_app: "vscode".to_string(),
        }
    }

    #[tokio::test]
    async fn copilot_token_refresh_state_uses_refresh_window() {
        let now = chrono::Utc::now().timestamp();
        let fresh = auth_with_token(Some(CopilotToken {
            token: "fresh".to_string(),
            expires_at: now + 3600,
            refresh_in: 1500,
        }));
        let stale = auth_with_token(Some(CopilotToken {
            token: "stale".to_string(),
            expires_at: now + 1200,
            refresh_in: 1500,
        }));
        let missing = auth_with_token(None);

        assert!(!fresh.copilot_token_needs_refresh().await);
        assert!(stale.copilot_token_needs_refresh().await);
        assert!(missing.copilot_token_needs_refresh().await);
    }

    #[tokio::test]
    async fn current_unexpired_copilot_token_rejects_expired_token() {
        let now = chrono::Utc::now().timestamp();
        let valid = auth_with_token(Some(CopilotToken {
            token: "valid".to_string(),
            expires_at: now + 60,
            refresh_in: 1500,
        }));
        let expired = auth_with_token(Some(CopilotToken {
            token: "expired".to_string(),
            expires_at: now - 1,
            refresh_in: 1500,
        }));

        assert_eq!(
            valid.current_unexpired_copilot_token().await.as_deref(),
            Some("valid")
        );
        assert_eq!(expired.current_unexpired_copilot_token().await, None);
    }
}
