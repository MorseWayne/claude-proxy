use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use claude_proxy_core::ModelInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::warn;

const CACHE_VERSION: u32 = 2;
const DEFAULT_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;
static CACHE_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Default, Serialize, Deserialize)]
struct CapabilityCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: Vec<CapabilityCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CapabilityCacheEntry {
    provider_id: String,
    base_url: String,
    identity_hash: String,
    model_id: String,
    context_window: u32,
    updated_at_unix_secs: u64,
}

pub(super) fn account_hash(account_id: Option<&str>) -> Option<String> {
    let account_id = account_id
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let digest = Sha256::digest(account_id.as_bytes());
    Some(format!("{digest:x}"))
}

pub fn current_account_hash() -> Option<String> {
    let token_path = dirs::config_dir()?
        .join("claude-proxy")
        .join("chatgpt")
        .join("token.json");
    let token: Value = serde_json::from_str(&fs::read_to_string(token_path).ok()?).ok()?;
    account_hash(token.get("account_id").and_then(Value::as_str))
}

/// Identity for persisted model capabilities, including available user/plan claims.
pub fn current_model_identity(
    runtime: &claude_proxy_config::settings::ProviderRuntimeConfig,
) -> Option<String> {
    let token_path = dirs::config_dir()?
        .join("claude-proxy")
        .join("chatgpt")
        .join("token.json");
    let token: super::auth::ChatGptToken =
        serde_json::from_str(&fs::read_to_string(token_path).ok()?).ok()?;
    Some(super::model_identity::with_routing_headers(
        &token.model_cache_identity(),
        runtime,
    ))
}

pub fn cached_context_window(
    provider_id: &str,
    base_url: &str,
    identity_hash: &str,
    model_id: &str,
) -> Option<u32> {
    let cache = load_cache()?;
    find_cached_context_window(
        &cache,
        provider_id,
        base_url,
        identity_hash,
        model_id,
        unix_timestamp_secs(),
    )
}

fn find_cached_context_window(
    cache: &CapabilityCache,
    provider_id: &str,
    base_url: &str,
    identity_hash: &str,
    model_id: &str,
    now: u64,
) -> Option<u32> {
    cache
        .entries
        .iter()
        .filter(|entry| {
            entry.provider_id == provider_id
                && entry.base_url == normalize_base_url(base_url)
                && entry.identity_hash == identity_hash
                && entry.model_id == model_id
                && now.saturating_sub(entry.updated_at_unix_secs) <= DEFAULT_RETENTION_SECS
        })
        .max_by_key(|entry| entry.updated_at_unix_secs)
        .map(|entry| entry.context_window)
}

pub(super) fn store_model_capabilities(
    provider_id: &str,
    base_url: &str,
    identity_hash: &str,
    models: &[ModelInfo],
) {
    let Some(path) = cache_path() else {
        return;
    };
    let _guard = CACHE_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = unix_timestamp_secs();
    let mut cache = load_cache().unwrap_or_default();
    cache.version = CACHE_VERSION;
    cache
        .entries
        .retain(|entry| now.saturating_sub(entry.updated_at_unix_secs) <= DEFAULT_RETENTION_SECS);

    let base_url = normalize_base_url(base_url);
    for model in models {
        let Some(context_window) = model.capabilities.limits.context_window else {
            continue;
        };
        cache.entries.retain(|entry| {
            !(entry.provider_id == provider_id
                && entry.base_url == base_url
                && entry.identity_hash == identity_hash
                && entry.model_id == model.model_id)
        });
        cache.entries.push(CapabilityCacheEntry {
            provider_id: provider_id.to_string(),
            base_url: base_url.clone(),
            identity_hash: identity_hash.to_string(),
            model_id: model.model_id.clone(),
            context_window,
            updated_at_unix_secs: now,
        });
    }

    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        warn!(%error, "Failed to create ChatGPT capability cache directory");
        return;
    }
    if let Err(error) = write_cache_atomically(&path, &cache) {
        warn!(%error, "Failed to persist ChatGPT capability cache");
    }
}

fn load_cache() -> Option<CapabilityCache> {
    load_cache_from_path(&cache_path()?)
}

fn load_cache_from_path(path: &Path) -> Option<CapabilityCache> {
    let cache: CapabilityCache = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    (cache.version == CACHE_VERSION).then_some(cache)
}

fn write_cache_atomically(path: &Path, cache: &CapabilityCache) -> std::io::Result<()> {
    let temp_path = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = serde_json::to_vec_pretty(cache)
        .map_err(std::io::Error::other)
        .and_then(|body| fs::write(&temp_path, body))
        .and_then(|_| fs::rename(&temp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
}

fn cache_path() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("claude-proxy")
            .join("chatgpt")
            .join("model-capabilities.json"),
    )
}

fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    reqwest::Url::parse(trimmed)
        .map(|url| url.to_string().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| trimmed.to_string())
}

fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_hash_is_stable_and_does_not_expose_account_id() {
        let first = account_hash(Some("account-123")).unwrap();
        let second = account_hash(Some("account-123")).unwrap();
        assert_eq!(first, second);
        assert!(!first.contains("account-123"));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn cache_lookup_is_scoped_and_rejects_stale_entries() {
        let now = 10_000_000;
        let cache = CapabilityCache {
            version: CACHE_VERSION,
            entries: vec![CapabilityCacheEntry {
                provider_id: "chatgpt".to_string(),
                base_url: "https://example.test/codex".to_string(),
                identity_hash: "account-hash".to_string(),
                model_id: "gpt-5.6-sol".to_string(),
                context_window: 372_000,
                updated_at_unix_secs: now,
            }],
        };

        assert_eq!(
            find_cached_context_window(
                &cache,
                "chatgpt",
                "https://EXAMPLE.test/codex/",
                "account-hash",
                "gpt-5.6-sol",
                now,
            ),
            Some(372_000)
        );
        assert_eq!(
            find_cached_context_window(
                &cache,
                "chatgpt",
                "https://example.test/Codex",
                "account-hash",
                "gpt-5.6-sol",
                now,
            ),
            None
        );
        assert_eq!(
            find_cached_context_window(
                &cache,
                "chatgpt",
                "https://example.test/codex",
                "other-account",
                "gpt-5.6-sol",
                now,
            ),
            None
        );
        assert_eq!(
            find_cached_context_window(
                &cache,
                "chatgpt",
                "https://example.test/codex",
                "account-hash",
                "gpt-5.6-sol",
                now + DEFAULT_RETENTION_SECS + 1,
            ),
            None
        );
    }

    #[test]
    fn cache_file_is_written_atomically_and_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "claude-proxy-capability-cache-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let cache = CapabilityCache {
            version: CACHE_VERSION,
            entries: vec![CapabilityCacheEntry {
                provider_id: "chatgpt".to_string(),
                base_url: "https://example.test/codex".to_string(),
                identity_hash: "hash-only".to_string(),
                model_id: "gpt-5.6-luna".to_string(),
                context_window: 372_000,
                updated_at_unix_secs: unix_timestamp_secs(),
            }],
        };

        write_cache_atomically(&path, &cache).unwrap();
        let loaded = load_cache_from_path(&path).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].identity_hash, "hash-only");
        assert_eq!(loaded.entries[0].context_window, 372_000);
        assert!(!fs::read_to_string(&path).unwrap().contains("raw-account"));
        let mut legacy = serde_json::to_value(&cache).unwrap();
        legacy["version"] = serde_json::json!(1);
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            load_cache_from_path(&path).is_none(),
            "legacy caches lack the required identity scope"
        );
        let _ = fs::remove_file(path);
    }
}
