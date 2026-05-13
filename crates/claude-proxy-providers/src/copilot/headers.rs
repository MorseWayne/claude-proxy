use sha2::{Digest, Sha256};
use uuid::Uuid;

const COPILOT_VERSION: &str = "0.46.0";
const VSCODE_VERSION: &str = "1.118.0";
const API_VERSION: &str = "2025-10-01";

/// Builds VS Code impersonation headers for Copilot API requests.
#[derive(Debug, Clone)]
pub struct HeaderBuilder {
    machine_id: String,
    device_id: String,
    session_id: String,
    user_agent: String,
    editor_plugin_version: String,
}

impl HeaderBuilder {
    pub fn new() -> Self {
        let machine_id = Self::generate_machine_id();
        let device_id = Uuid::new_v4().to_string();
        let session_id = format!(
            "{}{:x}",
            Uuid::new_v4().to_string().replace('-', ""),
            chrono::Utc::now().timestamp_millis()
        );

        Self {
            machine_id,
            device_id,
            session_id,
            user_agent: format!("GitHubCopilotChat/{COPILOT_VERSION}"),
            editor_plugin_version: format!("copilot-chat/{COPILOT_VERSION}"),
        }
    }

    fn generate_machine_id() -> String {
        if let Some(mac) = get_first_mac() {
            let mut hasher = Sha256::new();
            hasher.update(mac.as_bytes());
            format!("{:x}", hasher.finalize())
        } else {
            Uuid::new_v4().to_string().replace('-', "")
        }
    }

    #[allow(dead_code)]
    pub fn refresh_session_id(&mut self) {
        self.session_id = format!(
            "{}{:x}",
            Uuid::new_v4().to_string().replace('-', ""),
            chrono::Utc::now().timestamp_millis()
        );
    }

    pub fn build_headers(
        &self,
        token: &str,
        request_id: Option<&str>,
        vision: bool,
    ) -> Vec<(&'static str, String)> {
        let request_id = request_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut headers: Vec<(&'static str, String)> = vec![
            ("Authorization", format!("Bearer {token}")),
            ("Content-Type", "application/json".to_string()),
            ("copilot-integration-id", "vscode-chat".to_string()),
            ("editor-device-id", self.device_id.clone()),
            ("editor-version", format!("vscode/{VSCODE_VERSION}")),
            ("editor-plugin-version", self.editor_plugin_version.clone()),
            ("User-Agent", self.user_agent.clone()),
            ("openai-intent", "conversation-agent".to_string()),
            ("x-github-api-version", API_VERSION.to_string()),
            ("x-request-id", request_id.clone()),
            ("x-agent-task-id", request_id),
            ("x-interaction-type", "conversation-agent".to_string()),
            ("x-vscode-user-agent-library-version", "electron-fetch".to_string()),
        ];

        headers.push(("vscode-machineid", self.machine_id.clone()));
        headers.push(("vscode-sessionid", self.session_id.clone()));

        if vision {
            headers.push(("copilot-vision-request", "true".to_string()));
        }

        headers
    }

    pub fn build_models_headers(&self, token: &str) -> Vec<(&'static str, String)> {
        let mut headers = self.build_headers(token, None, false);
        // Override intent headers for model listing
        headers.retain(|(k, _)| {
            *k != "openai-intent"
                && *k != "x-interaction-type"
                && *k != "x-interaction-id"
                && *k != "x-request-id"
                && *k != "x-agent-task-id"
                && *k != "Content-Type"
        });
        headers.push(("openai-intent", "model-access".to_string()));
        headers.push(("x-interaction-type", "model-access".to_string()));
        headers
    }
}

fn get_first_mac() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let path = entry.path().join("address");
                if let Ok(addr) = fs::read_to_string(&path) {
                    let addr = addr.trim().to_string();
                    if addr != "00:00:00:00:00:00" && !addr.is_empty() {
                        return Some(addr);
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        if let Ok(output) = Command::new("ifconfig")
            .args(["en0"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.contains("ether") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 6 {
                        return Some(parts[1..7].join(":"));
                    }
                }
            }
        }
    }
    None
}
