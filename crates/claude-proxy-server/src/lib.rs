//! Axum HTTP server for claude-proxy.

pub mod app;
pub mod middleware;
pub mod routes;

pub use app::AppState;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post, put};
use claude_proxy_config::Settings;
use middleware::{RateLimitConfig, RateLimitLayer};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: AppState, settings: &Settings) -> Router {
    let rate_limit_layer = RateLimitLayer::new(RateLimitConfig {
        max_requests: settings.limits.rate_limit,
        per_seconds: settings.limits.rate_window,
    });

    Router::new()
        .route("/health", get(routes::health))
        .route("/v1/messages", post(routes::messages))
        .route("/v1/models", get(routes::list_models))
        .route("/admin/config", get(routes::admin_get_config))
        .route("/admin/config", put(routes::admin_update_config))
        .route("/admin/restart", post(routes::admin_restart))
        .route("/admin/metrics", get(routes::admin_metrics))
        .with_state(state)
        .layer(rate_limit_layer)
        .layer(TraceLayer::new_for_http())
}

/// Run the server with the given settings.
pub async fn run(settings: Settings) -> anyhow::Result<()> {
    let host = settings.server.host.clone();
    let port = settings.server.port;

    let state = AppState::new(settings.clone());
    let router = build_router(state.clone(), &settings);

    // Spawn config file watcher
    let _watcher = spawn_config_watcher(state.settings.clone(), state.provider_registry.clone());

    // Spawn SIGUSR1 handler for in-process config reload
    spawn_sigusr1_handler(state.settings.clone(), state.provider_registry.clone());

    // Spawn model cache warmup
    spawn_model_warmup(state.settings.clone(), state.provider_registry.clone());

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    info!("Listening on http://{addr}");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shut down");
    Ok(())
}

/// Spawn a background task to warm up model cache for all configured providers.
fn spawn_model_warmup(
    settings: Arc<RwLock<Settings>>,
    registry: Arc<RwLock<app::ProviderRegistry>>,
) {
    tokio::spawn(async move {
        // Small delay to let the server start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let provider_ids: Vec<String> = {
            let s = settings.read().await;
            s.providers.keys().cloned().collect()
        };

        for provider_id in &provider_ids {
            // Create provider and fetch models
            let result = {
                let mut reg = registry.write().await;
                let s = settings.read().await;
                match reg.get_or_create(provider_id, &s).await {
                    Ok(provider) => {
                        let models = provider.list_models().await;
                        Some(models)
                    }
                    Err(e) => {
                        warn!("Failed to create provider '{provider_id}' for warmup: {e}");
                        None
                    }
                }
            };

            if let Some(Ok(models)) = result {
                let mut reg = registry.write().await;
                reg.cache_models(provider_id, models.clone());
                info!(
                    "Warmed up model cache for '{}': {} models",
                    provider_id,
                    models.len()
                );
            } else if let Some(Err(e)) = result {
                warn!("Failed to fetch models for '{provider_id}': {e}");
            }
        }

        info!("Model cache warmup complete");
    });
}

/// Spawn a background task that watches the config file for changes.
/// Returns the watcher handle (dropped on shutdown).
fn spawn_config_watcher(
    settings: Arc<RwLock<Settings>>,
    registry: Arc<RwLock<app::ProviderRegistry>>,
) -> Option<RecommendedWatcher> {
    let config_path = Settings::config_file_path()?;
    if !config_path.exists() {
        warn!(
            "Config file not found at {}, skipping file watcher",
            config_path.display()
        );
        return None;
    }

    let watch_path = config_path.clone();
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = match RecommendedWatcher::new(tx, notify::Config::default()) {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to create config file watcher: {e}");
            return None;
        }
    };

    if let Err(e) = watcher.watch(&watch_path, RecursiveMode::NonRecursive) {
        warn!("Failed to watch config file {}: {e}", watch_path.display());
        return None;
    }

    info!("Watching config file for changes: {}", watch_path.display());

    // Spawn a task to handle file change events
    tokio::spawn(async move {
        let mut last_reload = std::time::Instant::now();
        let debounce = std::time::Duration::from_secs(2);

        loop {
            // Check for events (non-blocking poll with timeout)
            match rx.try_recv() {
                Ok(Ok(event)) => {
                    // Only react to modify events
                    if matches!(event.kind, EventKind::Modify(_)) {
                        // Debounce: skip if reloaded recently
                        if last_reload.elapsed() < debounce {
                            continue;
                        }
                        last_reload = std::time::Instant::now();

                        info!("Config file changed, reloading...");
                        match Settings::load(&config_path) {
                            Ok(new_settings) => {
                                if let Err(e) = new_settings.validate() {
                                    error!("Config validation failed after reload: {e}");
                                    continue;
                                }
                                let mut s = settings.write().await;
                                *s = new_settings;
                                let mut reg = registry.write().await;
                                reg.clear();
                                info!("Config reloaded successfully");
                            }
                            Err(e) => {
                                error!("Failed to reload config: {e}");
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("Config watcher error: {e}");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // No event, sleep briefly
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    info!("Config watcher channel disconnected");
                    break;
                }
            }
        }
    });

    Some(watcher)
}

/// Spawn a background task that listens for SIGUSR1 and reloads config.
#[cfg(unix)]
fn spawn_sigusr1_handler(
    settings: Arc<RwLock<Settings>>,
    registry: Arc<RwLock<app::ProviderRegistry>>,
) {
    tokio::spawn(async move {
        let mut sigusr1 =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("failed to install SIGUSR1 handler");

        while sigusr1.recv().await.is_some() {
            info!("Received SIGUSR1, reloading config...");
            if let Some(path) = Settings::config_file_path() {
                match Settings::load(&path) {
                    Ok(new_settings) => {
                        if let Err(e) = new_settings.validate() {
                            error!("Config validation failed after SIGUSR1 reload: {e}");
                            continue;
                        }
                        let mut s = settings.write().await;
                        *s = new_settings;
                        let mut reg = registry.write().await;
                        reg.clear();
                        info!("Config reloaded via SIGUSR1");
                    }
                    Err(e) => {
                        error!("Failed to reload config via SIGUSR1: {e}");
                    }
                }
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_sigusr1_handler(
    _settings: Arc<RwLock<Settings>>,
    _registry: Arc<RwLock<app::ProviderRegistry>>,
) {
    // No-op on non-Unix platforms
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => info!("Received SIGINT"),
            _ = sigterm.recv() => info!("Received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
        info!("Received SIGINT");
    }
}
