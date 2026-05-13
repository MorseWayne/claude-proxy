use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::time::{interval, sleep};
use tracing::{error, info, warn};

use crate::provider::ProviderError;

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_SCOPES: &str = "read:user";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

const POLL_INTERVAL_SECS: u64 = 5;
const MAX_POLL_ATTEMPTS: u32 = 60;

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
        let resp = client
            .post(GITHUB_DEVICE_CODE_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "client_id": GITHUB_CLIENT_ID,
                "scope": GITHUB_SCOPES,
            }))
            .send()
            .await
            .map_err(|e| ProviderError::Network(format!("device code request failed: {e}")))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Authentication(format!(
                "device code request rejected: {body}"
            )));
        }

        resp.json::<DeviceCodeResponse>()
            .await
            .map_err(|e| ProviderError::Network(format!("invalid device code response: {e}")))
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
                .map_err(|e| ProviderError::Network(format!("token poll failed: {e}")))?;

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
            .map_err(|e| ProviderError::Network(format!("copilot token request failed: {e}")))?;

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

        let data: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Network(format!("invalid copilot token response: {e}")))?;

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
        // If no GitHub token, run device flow
        if !self.has_github_token().await {
            let _token = self.run_device_flow().await?;
            self.refresh_copilot_token().await?;
            // Return the copilot token
            return self
                .copilot_token
                .read()
                .await
                .as_ref()
                .map(|t| t.token.clone())
                .ok_or_else(|| ProviderError::Authentication("no copilot token".to_string()));
        }

        // If no copilot token or expired, refresh
        let needs_refresh = {
            let ct = self.copilot_token.read().await;
            ct.as_ref().is_none_or(|t| {
                let now = chrono::Utc::now().timestamp();
                now + t.refresh_in >= t.expires_at
            })
        };

        if needs_refresh && let Err(e) = self.refresh_copilot_token().await {
            warn!("Failed to refresh Copilot token: {e}");
            // Return existing token if still valid
            let ct = self.copilot_token.read().await;
            if let Some(t) = ct.as_ref() {
                let now = chrono::Utc::now().timestamp();
                if now < t.expires_at {
                    return Ok(t.token.clone());
                }
            }
            return Err(e);
        }

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
