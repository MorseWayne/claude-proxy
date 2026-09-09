//! Model cache partitioning only; decoding claims here does not validate a token.

use super::auth::ChatGptToken;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) fn with_routing_headers(
    identity: &str,
    runtime: &claude_proxy_config::settings::ProviderRuntimeConfig,
) -> String {
    let headers = runtime
        .request
        .extra_headers
        .iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    format!(
        "{:x}",
        Sha256::digest(json!([identity, headers]).to_string().as_bytes())
    )
}

impl ChatGptToken {
    pub(super) fn model_cache_identity(&self) -> String {
        let claims = decode_claims(&self.access_token);
        let auth = claims
            .as_ref()
            .and_then(|claims| claims.get("https://api.openai.com/auth"));
        let account = self.account_id.as_deref().filter(|id| !id.is_empty());
        let claim_account = auth.and_then(|auth| auth["chatgpt_account_id"].as_str());
        let user = auth
            .and_then(|auth| auth["chatgpt_user_id"].as_str())
            .filter(|id| !id.is_empty());
        let email = claims
            .as_ref()
            .and_then(|claims| {
                claims["email"]
                    .as_str()
                    .or_else(|| claims["https://api.openai.com/profile"]["email"].as_str())
            })
            .filter(|email| !email.is_empty());
        let plan = auth.and_then(|auth| auth["chatgpt_plan_type"].as_str());
        let identity =
            if account.is_some() && account == claim_account && (user.is_some() || email.is_some())
            {
                // Do not invalidate a known owner's catalog for ordinary token rotation.
                json!(["chatgpt-models-v1", account, user, email, plan])
            } else {
                // Opaque or incomplete credentials cannot establish stable ownership.
                json!(["chatgpt-models-opaque-v1", account, self.access_token])
            };
        format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
    }
}

fn decode_claims(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(account: &str, user: &str, plan: &str, signature: &str) -> ChatGptToken {
        let claims = json!({"https://api.openai.com/auth": {
            "chatgpt_account_id":account, "chatgpt_user_id":user, "chatgpt_plan_type":plan
        }});
        ChatGptToken {
            access_token: format!(
                "header.{}.{signature}",
                URL_SAFE_NO_PAD.encode(claims.to_string())
            ),
            refresh_token: "unused".to_string(),
            expires_at: i64::MAX,
            account_id: Some(account.to_string()),
        }
    }

    #[test]
    fn model_identity_tracks_owner_and_plan_but_not_token_rotation() {
        let original = token("account", "user", "pro", "first");
        let identity = original.model_cache_identity();
        assert_eq!(
            identity,
            token("account", "user", "pro", "second").model_cache_identity()
        );
        for changed in [
            token("other", "user", "pro", "first"),
            token("account", "other", "pro", "first"),
            token("account", "user", "team", "first"),
        ] {
            assert_ne!(identity, changed.model_cache_identity());
        }
        let mut mismatched = original.clone();
        mismatched.account_id = Some("another-workspace".to_string());
        assert_ne!(identity, mismatched.model_cache_identity());
        let mut opaque = original;
        opaque.access_token = "opaque-first".to_string();
        let opaque_identity = opaque.model_cache_identity();
        opaque.access_token = "opaque-second".to_string();
        assert_ne!(opaque_identity, opaque.model_cache_identity());
        assert!(!identity.contains("account"));
    }

    #[test]
    fn model_identity_includes_effective_routing_headers() {
        let identity = token("account", "user", "pro", "first").model_cache_identity();
        let mut runtime = claude_proxy_config::settings::ProviderRuntimeConfig::default();
        let original = with_routing_headers(&identity, &runtime);
        runtime
            .request
            .extra_headers
            .insert("openai-project".to_string(), "project-a".to_string());
        let project_a = with_routing_headers(&identity, &runtime);
        assert_ne!(original, project_a);
        runtime
            .request
            .extra_headers
            .insert("openai-project".to_string(), "project-b".to_string());
        assert_ne!(project_a, with_routing_headers(&identity, &runtime));
    }
}
