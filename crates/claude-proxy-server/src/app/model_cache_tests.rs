use super::*;
use claude_proxy_core::{MessagesRequest, ModelInfo};
use claude_proxy_providers::{Provider, ProviderError, ProviderEvent};
use futures::{StreamExt, stream::BoxStream};
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

struct CatalogProvider {
    identity: StdMutex<String>,
    blocked: AtomicBool,
    started: Notify,
    finish: Notify,
}

impl CatalogProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            identity: StdMutex::new("account-a".to_string()),
            blocked: AtomicBool::new(false),
            started: Notify::new(),
            finish: Notify::new(),
        })
    }
}

#[async_trait::async_trait]
impl Provider for CatalogProvider {
    fn id(&self) -> &str {
        "catalog"
    }

    fn model_cache_identity(&self) -> Option<String> {
        Some(self.identity.lock().unwrap().clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let identity = self.model_cache_identity().unwrap();
        if self.blocked.load(Ordering::SeqCst) {
            self.started.notify_one();
            self.finish.notified().await;
        }
        Ok(vec![ModelInfo {
            model_id: identity,
            vendor: None,
            is_chat_default: None,
            capabilities: Default::default(),
        }])
    }

    async fn chat(
        &self,
        _: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        Ok(futures::stream::empty().boxed())
    }
}

#[tokio::test]
async fn model_cache_identity_change_invalidates_all_catalog_views() {
    let state = AppState::new(Settings::default(), None);
    let provider = CatalogProvider::new();
    state
        .provider_registry
        .write()
        .await
        .insert_if_absent("catalog", provider.clone());
    assert_eq!(
        state.get_or_refresh_models("catalog").await.unwrap()[0].model_id,
        "account-a"
    );
    *provider.identity.lock().unwrap() = "account-b".to_string();
    {
        let registry = state.provider_registry.read().await;
        assert!(registry.cached_models("catalog").is_none());
        assert!(registry.all_cached_models().is_empty());
        assert!(registry.all_cached_models_with_provider().is_empty());
        assert_eq!(registry.model_capabilities(), json!({}));
        assert!(!registry.model_cache_status(&["catalog".to_string()])[0].cached);
    }
    assert_eq!(
        state.get_or_refresh_models("catalog").await.unwrap()[0].model_id,
        "account-b"
    );
}

#[tokio::test]
async fn model_cache_rejects_refresh_finishing_after_identity_or_provider_change() {
    for replace_provider in [false, true] {
        let state = AppState::new(Settings::default(), None);
        let provider = CatalogProvider::new();
        provider.blocked.store(true, Ordering::SeqCst);
        state
            .provider_registry
            .write()
            .await
            .insert_if_absent("catalog", provider.clone());
        let fetch_state = state.clone();
        let fetch = tokio::spawn(async move { fetch_state.get_or_refresh_models("catalog").await });
        tokio::time::timeout(Duration::from_secs(2), provider.started.notified())
            .await
            .unwrap();
        if replace_provider {
            let mut registry = state.provider_registry.write().await;
            registry.clear();
            registry.insert_if_absent("catalog", CatalogProvider::new());
        } else {
            *provider.identity.lock().unwrap() = "account-b".to_string();
        }
        provider.finish.notify_one();
        assert!(
            fetch
                .await
                .unwrap()
                .unwrap_err()
                .contains("changed during refresh")
        );
        assert!(
            state
                .provider_registry
                .read()
                .await
                .all_cached_models()
                .is_empty()
        );
        provider.blocked.store(false, Ordering::SeqCst);
        let models = state.get_or_refresh_models("catalog").await.unwrap();
        assert_eq!(
            models[0].model_id,
            if replace_provider {
                "account-a"
            } else {
                "account-b"
            }
        );
    }
}
