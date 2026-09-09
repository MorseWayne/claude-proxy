//! Integration tests for claude-proxy server.

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use claude_proxy_config::Settings;
use claude_proxy_config::settings::{
    AdminConfig, HttpConfig, LimitsConfig, LogConfig, ModelAliasConfig, ModelConfig,
    ObservabilityConfig, ProviderConfig, ProviderType, ServerConfig,
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
            provider_type: Some(ProviderType::OpenAI),
            copilot: None,
            chatgpt: None,
            runtime: Default::default(),
            reasoning_markers: Default::default(),
        },
    );

    Settings {
        providers,
        model: ModelConfig {
            default: ModelAliasConfig::new("openai/gpt-4"),
            reasoning: None,
            opus: None,
            sonnet: None,
            haiku: None,
        },
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            auth_token: auth_token.to_string(),
            ..ServerConfig::default()
        },
        admin: AdminConfig { auth_token: None },
        limits: LimitsConfig {
            rate_limit: 100,
            rate_window: 60,
            max_concurrency: 10,
            max_concurrency_queue: 32,
            provider_max_concurrency: 10,
            provider_max_concurrency_queue: 16,
            model_cache_ttl_seconds: 3600,
            max_non_stream_response_bytes: 32 * 1024 * 1024,
        },
        http: HttpConfig::default(),
        log: LogConfig::default(),
        observability: ObservabilityConfig::default(),
    }
}

/// Start a mock OpenAI server that returns SSE responses.
/// Returns the base URL of the mock server.
async fn start_mock_openai() -> String {
    let app = Router::new()
        .route("/chat/completions", post(mock_chat_completions))
        .route("/responses", post(mock_responses))
        .route("/models", get(mock_models));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    base_url
}

/// Mock /chat/completions endpoint.
async fn mock_chat_completions(Json(payload): Json<serde_json::Value>) -> Response {
    if !payload["stream"].as_bool().unwrap_or(false) {
        return Json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello world"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 4,
                "completion_tokens": 2,
                "total_tokens": 6
            }
        }))
        .into_response();
    }

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

/// Mock native /responses endpoint. Assertions here verify that the proxy did
/// not translate or discard Codex-specific input items.
async fn mock_responses(headers: HeaderMap, Json(payload): Json<serde_json::Value>) -> Response {
    assert_eq!(headers["authorization"], "Bearer test-key");
    assert_eq!(headers["x-codex-turn-state"], "turn-state-1");
    assert!(headers.get("x-api-key").is_none());
    assert!(headers.get("cookie").is_none());
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["store"], false);
    assert!(payload.get("background").is_none());
    assert_eq!(payload["future_option"], json!({"enabled": true}));
    if payload
        .pointer("/metadata/verify_structured_outputs")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        assert_eq!(payload["max_output_tokens"], 512);
        assert_eq!(payload["text"]["format"]["type"], "json_schema");
        assert_eq!(payload["text"]["format"]["name"], "verification");
        assert_eq!(
            payload["text"]["format"]["schema"]["additionalProperties"],
            false
        );
    }
    let input = payload["input"].as_array().expect("Responses input array");
    assert!(input.iter().any(|item| item["type"] == "additional_tools"));
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "configuration_update")
    );
    assert!(input.iter().any(|item| {
        item["type"] == "function_call_output"
            && item.get("call_id").is_none()
            && item["name"] == "notifications"
    }));

    let is_compaction = input
        .iter()
        .any(|item| item["type"] == "compaction_trigger");
    let force_failure = payload
        .pointer("/metadata/force_failure")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let sse_data = if force_failure {
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-failed\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\"}}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-failed\",\"model\":\"gpt-5.6-sol\",\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"retry in 2s\"}}}\n\n",
        )
    } else if is_compaction {
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-compact\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"cmp-1\",\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-compact\",\"model\":\"gpt-5.6-sol\",\"status\":\"completed\",\"output\":[{\"id\":\"cmp-1\",\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}],\"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}\n\n",
        )
    } else {
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-test\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg-1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\",\"annotations\":[]}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-test\",\"model\":\"gpt-5.6-sol\",\"status\":\"completed\",\"output\":[{\"id\":\"msg-1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}\n\n",
        )
    };

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("x-request-id", "req-native")
        .header("openai-model", "gpt-5.6-sol")
        .header("x-codex-primary-used-percent", "42")
        .header("set-cookie", "upstream-secret=do-not-forward")
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
    let state = AppState::new(settings.clone(), None);
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
async fn astra_model_catalog_and_messages_use_responses_capabilities() {
    let mock = Router::new()
        .route("/models", get(|| async { Json(json!({"data":[{"id":"gpt-6-astra"}]})) }))
        .route("/responses", post(|Json(body): Json<serde_json::Value>| async move {
            assert_eq!(body["model"], "gpt-6-astra");
            assert_eq!(body["reasoning"]["effort"], "high");
            assert_eq!(body["text"]["verbosity"], "low");
            assert!(body.get("temperature").is_none());
            assert!(body.get("top_p").is_none());
            Response::builder().header("content-type", "text/event-stream").body(Body::from(concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-astra\",\"model\":\"gpt-6-astra\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello Astra\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-astra\",\"status\":\"completed\",\"output\":[]}}\n\n"
            ))).unwrap()
        }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let proxy = start_proxy(test_settings(&mock_url, "test-token")).await;
    let client = reqwest::Client::new();
    let catalog: serde_json::Value = client
        .get(format!("{proxy}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let model = &catalog["data"][0];
    assert_eq!(model["qualified_id"], "openai/gpt-6-astra");
    assert_eq!(
        model["supported_endpoints"],
        json!(["/chat/completions", "/responses"])
    );
    assert_eq!(
        model["capabilities"]["limits"],
        json!({
            "context_window": 1_050_000, "max_output_tokens": 128_000,
            "reasoning_effort_levels": ["low", "medium", "high", "xhigh", "max"]
        })
    );
    assert_eq!(model["capabilities"]["responses"]["streaming"], "required");
    assert_eq!(model["capabilities"]["features"]["sampling"], "unsupported");
    let response = client
        .post(format!("{proxy}/v1/messages"))
        .header("x-api-key", "test-token")
        .json(&json!({
            "model":"openai/gpt-6-astra", "stream":true,
            "messages":[{"role":"user", "content":"hello"}],
            "thinking":{"type":"adaptive"}, "verbosity":"low", "max_tokens":1024
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("Hello Astra"));
    upstream.abort();
}

fn native_codex_input(compaction: bool) -> Vec<serde_json::Value> {
    let mut input = vec![
        json!({
            "id": "at-stable",
            "type": "additional_tools",
            "role": "developer",
            "tools": [{"type": "function", "name": "weather"}]
        }),
        json!({
            "type": "configuration_update",
            "reasoning": {"effort": "high"}
        }),
        json!({
            "type": "function_call_output",
            "name": "notifications",
            "namespace": "slack",
            "output": "Alice mentioned you"
        }),
        json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Hi"}]
        }),
    ];
    if compaction {
        input.push(json!({"type": "compaction_trigger"}));
    }
    input
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
async fn test_non_streaming_messages_returns_complete_message() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy_url))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "Hello world");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 4);
    assert_eq!(body["usage"]["output_tokens"], 2);
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

    let resp = client
        .get(format!("{}/v1/models", proxy_url))
        .header("x-api-key", "test-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][0]["id"], "gpt-4");
    assert_eq!(body["data"][0]["provider"], "openai");
    assert_eq!(body["data"][0]["qualified_id"], "openai/gpt-4");
    assert_eq!(body["data"][1]["id"], "gpt-4-mini");
}

#[tokio::test]
async fn test_admin_models_refresh_with_auth() {
    let mock_url = start_mock_openai().await;
    let mut settings = test_settings(&mock_url, "test-token");
    settings.admin.auth_token = Some("admin-secret".to_string());
    let proxy_url = start_proxy(settings).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/models/refresh", proxy_url))
        .header("authorization", "Bearer admin-secret")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["refreshed"]["openai"], 2);
    assert_eq!(body["model_cache"][0]["provider"], "openai");
    assert_eq!(body["model_cache"][0]["cached"], true);
    assert_eq!(body["model_cache"][0]["model_count"], 2);
    assert_eq!(body["model_cache"][0]["fresh"], true);
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

#[tokio::test]
async fn test_openai_chat_completions_non_streaming() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("authorization", "Bearer test-token")
        .json(&json!({
            "model": "gpt-4",
            "messages": [
                {"role": "developer", "content": "Be concise"},
                {"role": "user", "content": "Hi"}
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "gpt-4");
    assert_eq!(body["choices"][0]["message"]["role"], "assistant");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello world");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 4);
    assert_eq!(body["usage"]["completion_tokens"], 2);
}

#[tokio::test]
async fn test_openai_chat_completions_streaming() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("authorization", "Bearer test-token")
        .json(&json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.text().await.unwrap();
    assert!(body.contains("\"object\":\"chat.completion.chunk\""));
    assert!(body.contains("\"content\":\"Hello\""));
    assert!(body.contains("\"content\":\" world\""));
    assert!(body.contains("\"choices\":[]"));
    assert!(body.contains("\"usage\":{"));
    assert!(body.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn test_openai_responses_requires_streaming() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-api-key", "test-token")
        .header("x-codex-turn-state", "turn-state-1")
        .header("cookie", "downstream-secret=do-not-forward")
        .json(&json!({
            "model": "gpt-4",
            "instructions": "Be concise",
            "input": "Hi"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["param"], "stream");
}

#[tokio::test]
async fn test_openai_responses_rejects_non_native_provider() {
    let mock_url = start_mock_openai().await;
    let mut settings = test_settings(&mock_url, "test-token");
    settings
        .providers
        .get_mut("openai")
        .expect("test provider")
        .provider_type = Some(ProviderType::Anthropic);
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-api-key", "test-token")
        .json(&json!({
            "model": "gpt-4",
            "input": "Hi",
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("native Responses"))
    );
}

#[tokio::test]
async fn test_openai_responses_streaming_preserves_structured_outputs_request() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-api-key", "test-token")
        .header("x-codex-turn-state", "turn-state-1")
        .header("cookie", "downstream-secret=do-not-forward")
        .json(&json!({
            "model": "gpt-4",
            "input": native_codex_input(false),
            "stream": true,
            "future_option": {"enabled": true},
            "max_output_tokens": 512,
            "metadata": {"verify_structured_outputs": true},
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "verification",
                    "schema": {
                        "type": "object",
                        "properties": {"ok": {"type": "boolean"}},
                        "required": ["ok"],
                        "additionalProperties": false
                    },
                    "strict": true
                }
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-request-id"], "req-native");
    assert_eq!(response.headers()["openai-model"], "gpt-5.6-sol");
    assert_eq!(response.headers()["x-codex-primary-used-percent"], "42");
    assert!(response.headers().get("set-cookie").is_none());
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.created"));
    assert!(body.contains("event: response.output_text.delta"));
    assert!(body.contains("\"delta\":\"Hello\""));
    assert!(body.contains("event: response.output_item.done"));
    assert!(body.contains("event: response.completed"));
    assert!(!body.contains("[DONE]"));
}

#[tokio::test]
async fn test_openai_responses_preserves_v2_compaction() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-api-key", "test-token")
        .header("x-codex-turn-state", "turn-state-1")
        .json(&json!({
            "model": "gpt-4",
            "input": native_codex_input(true),
            "stream": true,
            "future_option": {"enabled": true}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.output_item.done"));
    assert!(body.contains("\"type\":\"compaction\""));
    assert!(body.contains("\"encrypted_content\":\"opaque\""));
}

#[tokio::test]
async fn test_openai_responses_preserves_structured_failure() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-api-key", "test-token")
        .header("x-codex-turn-state", "turn-state-1")
        .json(&json!({
            "model": "gpt-4",
            "input": native_codex_input(false),
            "stream": true,
            "future_option": {"enabled": true},
            "metadata": {"force_failure": true}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.failed"));
    assert!(body.contains("\"code\":\"rate_limit_exceeded\""));
    assert!(body.contains("\"message\":\"retry in 2s\""));
    assert!(!body.contains("upstream_stream_error"));
}

#[tokio::test]
async fn test_responses_websocket_probe_returns_upgrade_required() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;

    let response = reqwest::Client::new()
        .get(format!("{proxy_url}/v1/responses"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
}

#[tokio::test]
async fn test_openai_endpoint_errors_use_openai_shape() {
    let mock_url = start_mock_openai().await;
    let settings = test_settings(&mock_url, "test-token");
    let proxy_url = start_proxy(settings).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("authorization", "Bearer wrong-token")
        .json(&json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = unauthorized.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_api_key");
    assert!(body.get("type").is_none());

    let stateful = client
        .post(format!("{proxy_url}/v1/responses"))
        .header("authorization", "Bearer test-token")
        .json(&json!({
            "model": "gpt-4",
            "input": "Hi",
            "stream": true,
            "previous_response_id": "resp_previous"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stateful.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = stateful.json().await.unwrap();
    assert_eq!(body["error"]["param"], "previous_response_id");
}
