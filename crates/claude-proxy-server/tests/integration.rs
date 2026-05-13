//! Integration tests for claude-proxy server.

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use claude_proxy_config::Settings;
use claude_proxy_config::settings::{
    AdminConfig, HttpConfig, LimitsConfig, LogConfig, ModelConfig, ProviderConfig, ServerConfig,
};
use claude_proxy_server::AppState;
use serde_json::json;
use tokio::net::TcpListener;

/// Create a test Settings pointing to the given upstream URL.
fn test_settings(upstream_url: &str, auth_token: &str) -> Settings {
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: upstream_url.to_string(),
            proxy: String::new(),
            copilot: None,
        },
    );

    Settings {
        providers,
        model: ModelConfig {
            default: "openai/gpt-4".to_string(),
            opus: None,
            sonnet: None,
            haiku: None,
        },
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            auth_token: auth_token.to_string(),
        },
        admin: AdminConfig { auth_token: None },
        limits: LimitsConfig {
            rate_limit: 100,
            rate_window: 60,
            max_concurrency: 10,
        },
        http: HttpConfig::default(),
        log: LogConfig::default(),
    }
}

/// Start a mock OpenAI server that returns SSE responses.
/// Returns the base URL of the mock server.
async fn start_mock_openai() -> String {
    let app = Router::new()
        .route("/chat/completions", post(mock_chat_completions))
        .route("/models", get(mock_models));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    base_url
}

/// Mock /chat/completions endpoint — returns a simple SSE stream.
async fn mock_chat_completions() -> Response {
    let sse_data = r#"data: {"id":"chatcmpl-test","object":"chat.completion.chunk","model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-test","object":"chat.completion.chunk","model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl-test","object":"chat.completion.chunk","model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(sse_data))
        .unwrap()
}

/// Mock /models endpoint.
async fn mock_models() -> Json<serde_json::Value> {
    Json(json!({
        "data": [
            {"id": "gpt-4", "object": "model"},
            {"id": "gpt-4-mini", "object": "model"}
        ],
        "object": "list"
    }))
}

/// Start the proxy server on a random port. Returns the base URL.
async fn start_proxy(settings: Settings) -> String {
    let state = AppState::new(settings.clone());
    let router = claude_proxy_server::build_router(state, &settings);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    base_url
}

#[tokio::test]
async fn test_health_endpoint() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/health", proxy_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn test_messages_with_valid_auth() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy_url))
        .header("x-api-key", "test-token")
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    // Should contain SSE events
    assert!(body.contains("event: message_start"));
    assert!(body.contains("event: content_block_start"));
    assert!(body.contains("event: message_stop"));
}

#[tokio::test]
async fn test_messages_with_invalid_auth() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy_url))
        .header("x-api-key", "wrong-token")
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn test_messages_no_auth_when_empty() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, ""); // empty auth token = no auth
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy_url))
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_list_models() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();

    // First, make a request to trigger provider creation and model fetch
    // (models are cached lazily)
    let _ = client
        .post(format!("{}/v1/messages", proxy_url))
        .header("x-api-key", "test-token")
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 10,
            "stream": false,
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .send()
        .await;

    // Now check models endpoint
    let resp = client
        .get(format!("{}/v1/models", proxy_url))
        .header("x-api-key", "test-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    // Models come from cache, which may be empty until list_models is called
}

#[tokio::test]
async fn test_admin_config_without_auth() {
    let mock_url = start_mock_openai().await;
    let mut settings = test_settings(&mock_url, "test-token");
    settings.admin.auth_token = Some("admin-secret".to_string());
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/config", proxy_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_admin_config_with_auth() {
    let mock_url = start_mock_openai().await;
    let mut settings = test_settings(&mock_url, "test-token");
    settings.admin.auth_token = Some("admin-secret".to_string());
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/config", proxy_url))
        .header("authorization", "Bearer admin-secret")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["config"].is_string());
    // Config should have masked keys
    let config_str = body["config"].as_str().unwrap();
    assert!(config_str.contains("***"));
}
