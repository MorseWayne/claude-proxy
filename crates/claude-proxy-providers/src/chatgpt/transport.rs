use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use base64::Engine;
use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::SseEvent;
use futures::{SinkExt, Stream, StreamExt, stream::BoxStream};
use reqwest::{
    StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{Error as WsError, Message, client::IntoClientRequest};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    connect_async_tls_with_config,
};
use tracing::info;

use super::{ChatGptProvider, ChatGptToken, map_chatgpt_error_status_body_with_headers};
use crate::provider::ProviderError;

const OPENAI_BETA_HEADER: &str = "openai-beta";
const RESPONSES_WEBSOCKETS_BETA: &str = "responses_websockets=2026-02-06";
const CHATGPT_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CHATGPT_WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const CHATGPT_WEBSOCKET_SESSION_IDLE_TTL: Duration = Duration::from_secs(300);
const CHATGPT_CONTINUATION_SCHEMA_VERSION: &str = "chatgpt-continuation-v1";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";

#[derive(Debug)]
pub(super) struct ChatGptWebSocketStartError {
    pub error: ProviderError,
    pub fallback_allowed: bool,
}

impl ChatGptWebSocketStartError {
    fn new(error: ProviderError, fallback_allowed: bool) -> Self {
        Self {
            error,
            fallback_allowed,
        }
    }
}

type ChatGptWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct AbortOnDropStream {
    inner: BoxStream<'static, Result<SseEvent, ProviderError>>,
    abort_tx: Option<watch::Sender<bool>>,
}

impl AbortOnDropStream {
    fn new(
        inner: BoxStream<'static, Result<SseEvent, ProviderError>>,
        abort_tx: watch::Sender<bool>,
    ) -> Self {
        Self {
            inner,
            abort_tx: Some(abort_tx),
        }
    }
}

impl Stream for AbortOnDropStream {
    type Item = Result<SseEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for AbortOnDropStream {
    fn drop(&mut self) {
        if let Some(abort_tx) = self.abort_tx.take() {
            let _ = abort_tx.send(true);
        }
    }
}

pub(super) struct ChatGptWebSocketSession {
    cached: Option<CachedWebSocketConnection>,
    continuations: HashMap<ContinuationKey, CachedContinuation>,
    busy: HashSet<ContinuationKey>,
    generations: HashMap<ContinuationKey, u64>,
}

impl ChatGptWebSocketSession {
    pub fn new() -> Self {
        Self {
            cached: None,
            continuations: HashMap::new(),
            busy: HashSet::new(),
            generations: HashMap::new(),
        }
    }

    fn take_fresh(&mut self) -> Option<ChatGptWsStream> {
        let cached = self.cached.take()?;
        if cached.last_used.elapsed() <= CHATGPT_WEBSOCKET_SESSION_IDLE_TTL {
            Some(cached.stream)
        } else {
            self.continuations.clear();
            self.busy.clear();
            None
        }
    }

    fn store_if_empty(&mut self, stream: ChatGptWsStream) -> bool {
        if self.cached.is_some() {
            return false;
        }
        self.cached = Some(CachedWebSocketConnection {
            stream,
            last_used: Instant::now(),
        });
        true
    }

    fn prepare_continuation(
        &mut self,
        provider: &ChatGptProvider,
        token: &ChatGptToken,
        body: &Value,
        stable_client_conversation_id: Option<&str>,
    ) -> ContinuationAttempt {
        self.prune_expired_continuations();
        let Some(key) = continuation_key(provider, token, body, stable_client_conversation_id)
        else {
            return ContinuationAttempt::disabled(
                body.clone(),
                ContinuationDisabledReason::MissingKey,
            );
        };
        self.clear_conflicting_continuations(&key);

        let Some(full_input) = response_input_items(body) else {
            self.clear_continuation_for_key(&key);
            self.bump_generation(&key);
            return ContinuationAttempt::disabled_with_key(
                key,
                body.clone(),
                ContinuationDisabledReason::InvalidInput,
            );
        };
        let canonical_body = canonical_request_body(body);

        if self.busy.contains(&key) {
            self.clear_continuation_for_key(&key);
            self.bump_generation(&key);
            return ContinuationAttempt::disabled_with_key(
                key,
                body.clone(),
                ContinuationDisabledReason::Busy,
            );
        }

        let generation = self.current_generation(&key);
        let mut send_body = body.clone();
        let mut used = false;
        let mut disabled_reason = ContinuationDisabledReason::MissingSession;
        match self.continuations.get(&key) {
            Some(cached) if cached.canonical_body != canonical_body => {
                self.clear_continuation_for_key(&key);
                disabled_reason = ContinuationDisabledReason::BodyMismatch;
            }
            Some(cached) if cached.response_id.is_empty() => {
                self.clear_continuation_for_key(&key);
                disabled_reason = ContinuationDisabledReason::MissingResponseId;
            }
            Some(cached) => {
                let mut baseline = cached.full_input.clone();
                baseline.extend(cached.assistant_output_items.clone());
                if full_input.starts_with(&baseline) && full_input.len() > baseline.len() {
                    let delta = full_input[baseline.len()..].to_vec();
                    if let Some(object) = send_body.as_object_mut() {
                        object.insert(
                            "previous_response_id".to_string(),
                            Value::String(cached.response_id.clone()),
                        );
                        object.insert("input".to_string(), Value::Array(delta));
                        used = true;
                        disabled_reason = ContinuationDisabledReason::None;
                    } else {
                        self.clear_continuation_for_key(&key);
                        self.bump_generation(&key);
                        return ContinuationAttempt::disabled_with_key(
                            key,
                            body.clone(),
                            ContinuationDisabledReason::InvalidBody,
                        );
                    }
                } else {
                    self.clear_continuation_for_key(&key);
                    disabled_reason = ContinuationDisabledReason::PrefixMismatch;
                }
            }
            None => {}
        }

        self.busy.insert(key.clone());
        ContinuationAttempt {
            key: Some(key),
            send_body,
            canonical_body,
            full_input,
            used,
            update_on_success: true,
            owns_busy: true,
            generation,
            disabled_reason,
        }
    }

    fn complete_continuation(&mut self, attempt: &ContinuationAttempt, terminal_event: &Value) {
        let Some(key) = attempt.key.as_ref() else {
            return;
        };
        if attempt.owns_busy {
            self.busy.remove(key);
        }

        if !attempt.update_on_success {
            return;
        }
        if self.current_generation(key) != attempt.generation {
            self.clear_continuation_for_key(key);
            return;
        }

        if !is_successful_completed_event(terminal_event) {
            self.clear_continuation_for_key(key);
            self.bump_generation(key);
            return;
        }

        let Some(response_id) = terminal_response_id(terminal_event) else {
            self.clear_continuation_for_key(key);
            self.bump_generation(key);
            return;
        };
        let Some(assistant_output_items) = terminal_assistant_output_items(terminal_event) else {
            self.clear_continuation_for_key(key);
            self.bump_generation(key);
            return;
        };

        self.continuations.insert(
            key.clone(),
            CachedContinuation {
                canonical_body: attempt.canonical_body.clone(),
                full_input: attempt.full_input.clone(),
                assistant_output_items,
                response_id: response_id.to_string(),
                updated_at: Instant::now(),
            },
        );
    }

    fn fail_continuation(&mut self, attempt: &ContinuationAttempt) {
        if let Some(key) = attempt.key.as_ref() {
            if attempt.owns_busy {
                self.busy.remove(key);
            }
            if attempt.update_on_success || attempt.owns_busy {
                self.clear_continuation_for_key(key);
                self.bump_generation(key);
            }
        }
    }

    fn clear_conflicting_continuations(&mut self, key: &ContinuationKey) {
        let conflicts = self
            .continuations
            .keys()
            .chain(self.busy.iter())
            .filter(|candidate| {
                candidate.provider_id == key.provider_id
                    && candidate.stable_client_conversation_id == key.stable_client_conversation_id
                    && *candidate != key
            })
            .cloned()
            .collect::<Vec<_>>();
        for conflict in conflicts {
            self.clear_continuation_for_key(&conflict);
            self.bump_generation(&conflict);
        }
    }

    fn clear_continuation_for_key(&mut self, key: &ContinuationKey) {
        self.continuations.remove(key);
    }

    fn current_generation(&self, key: &ContinuationKey) -> u64 {
        self.generations.get(key).copied().unwrap_or(0)
    }

    fn bump_generation(&mut self, key: &ContinuationKey) {
        *self.generations.entry(key.clone()).or_insert(0) += 1;
    }

    fn prune_expired_continuations(&mut self) {
        self.continuations
            .retain(|_, cached| cached.updated_at.elapsed() <= CHATGPT_WEBSOCKET_SESSION_IDLE_TTL);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContinuationKey {
    provider_id: String,
    account_id: String,
    model: String,
    stable_client_conversation_id: String,
    schema_version: &'static str,
}

#[derive(Debug, Clone)]
struct CachedContinuation {
    canonical_body: Value,
    full_input: Vec<Value>,
    assistant_output_items: Vec<Value>,
    response_id: String,
    updated_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuationDisabledReason {
    None,
    MissingKey,
    MissingSession,
    Busy,
    InvalidBody,
    InvalidInput,
    BodyMismatch,
    PrefixMismatch,
    MissingResponseId,
}

impl ContinuationDisabledReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MissingKey => "missing_key",
            Self::MissingSession => "missing_session",
            Self::Busy => "busy",
            Self::InvalidBody => "invalid_body",
            Self::InvalidInput => "invalid_input",
            Self::BodyMismatch => "body_mismatch",
            Self::PrefixMismatch => "prefix_mismatch",
            Self::MissingResponseId => "missing_response_id",
        }
    }
}

#[derive(Debug, Clone)]
struct ContinuationAttempt {
    key: Option<ContinuationKey>,
    send_body: Value,
    canonical_body: Value,
    full_input: Vec<Value>,
    used: bool,
    update_on_success: bool,
    owns_busy: bool,
    generation: u64,
    disabled_reason: ContinuationDisabledReason,
}

impl ContinuationAttempt {
    fn disabled(body: Value, disabled_reason: ContinuationDisabledReason) -> Self {
        Self {
            key: None,
            canonical_body: canonical_request_body(&body),
            full_input: response_input_items(&body).unwrap_or_default(),
            send_body: body,
            used: false,
            update_on_success: false,
            owns_busy: false,
            generation: 0,
            disabled_reason,
        }
    }

    fn disabled_with_key(
        key: ContinuationKey,
        body: Value,
        disabled_reason: ContinuationDisabledReason,
    ) -> Self {
        Self {
            key: Some(key),
            canonical_body: canonical_request_body(&body),
            full_input: response_input_items(&body).unwrap_or_default(),
            send_body: body,
            used: false,
            update_on_success: false,
            owns_busy: false,
            generation: 0,
            disabled_reason,
        }
    }
}

struct CachedWebSocketConnection {
    stream: ChatGptWsStream,
    last_used: Instant,
}

fn continuation_key(
    provider: &ChatGptProvider,
    token: &ChatGptToken,
    body: &Value,
    stable_client_conversation_id: Option<&str>,
) -> Option<ContinuationKey> {
    Some(ContinuationKey {
        provider_id: provider.id.clone(),
        account_id: token.account_id.as_deref()?.trim().to_string(),
        model: body.get("model")?.as_str()?.trim().to_string(),
        stable_client_conversation_id: stable_client_conversation_id?.trim().to_string(),
        schema_version: CHATGPT_CONTINUATION_SCHEMA_VERSION,
    })
    .filter(|key| {
        !key.provider_id.is_empty()
            && !key.account_id.is_empty()
            && !key.model.is_empty()
            && !key.stable_client_conversation_id.is_empty()
    })
}

fn response_input_items(body: &Value) -> Option<Vec<Value>> {
    body.get("input").and_then(Value::as_array).cloned()
}

fn canonical_request_body(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("input");
        object.remove("previous_response_id");
        object.remove("type");
    }
    canonical_json_value(body)
}

fn canonical_json_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(canonical_json_value).collect())
        }
        Value::Object(object) => {
            let sorted = object
                .into_iter()
                .map(|(key, value)| (key, canonical_json_value(value)))
                .collect::<BTreeMap<_, _>>();
            let mut mapped = Map::new();
            for (key, value) in sorted {
                mapped.insert(key, value);
            }
            Value::Object(mapped)
        }
        value => value,
    }
}

fn is_successful_completed_event(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("response.completed")
        && event
            .get("response")
            .and_then(|response| response.get("status"))
            .and_then(Value::as_str)
            .is_none_or(|status| status == "completed")
}

fn terminal_response_id(event: &Value) -> Option<&str> {
    event
        .get("response")
        .unwrap_or(event)
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn terminal_assistant_output_items(event: &Value) -> Option<Vec<Value>> {
    let output = event
        .get("response")
        .unwrap_or(event)
        .get("output")
        .and_then(Value::as_array)?;
    output
        .iter()
        .map(assistant_output_item_to_input_prefix_item)
        .collect()
}

fn assistant_output_item_to_input_prefix_item(item: &Value) -> Option<Value> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => assistant_message_output_to_input_item(item),
        Some("function_call") => assistant_function_call_output_to_input_item(item),
        Some("reasoning" | "custom_tool_call") => None,
        Some(_) | None => None,
    }
}

fn assistant_message_output_to_input_item(item: &Value) -> Option<Value> {
    let role = item
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    if role != "assistant" {
        return None;
    }
    let content = item.get("content")?.as_array()?;
    let mut text_parts = Vec::new();
    for part in content {
        match part.get("type").and_then(Value::as_str) {
            Some("output_text") => {
                text_parts.push(part.get("text").and_then(Value::as_str).unwrap_or_default());
            }
            Some("refusal") => {
                text_parts.push(
                    part.get("refusal")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            Some(_) | None => return None,
        }
    }
    Some(Value::Object(Map::from_iter([
        ("role".to_string(), Value::String("assistant".to_string())),
        ("content".to_string(), Value::String(text_parts.join("\n"))),
    ])))
}

fn assistant_function_call_output_to_input_item(item: &Value) -> Option<Value> {
    let call_id = item.get("call_id").and_then(Value::as_str)?;
    let name = item.get("name").and_then(Value::as_str)?;
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(Value::Object(Map::from_iter([
        (
            "type".to_string(),
            Value::String("function_call".to_string()),
        ),
        ("call_id".to_string(), Value::String(call_id.to_string())),
        ("name".to_string(), Value::String(name.to_string())),
        (
            "arguments".to_string(),
            Value::String(arguments.to_string()),
        ),
    ])))
}

pub(super) async fn open_websocket_stream<F>(
    provider: &ChatGptProvider,
    body: Value,
    token: &ChatGptToken,
    marker_mode: ReasoningMarkerMode,
    stable_client_conversation_id: Option<&str>,
    on_event: F,
) -> Result<(BoxStream<'static, Result<SseEvent, ProviderError>>, bool), ChatGptWebSocketStartError>
where
    F: Fn(&Value) + Send + Sync + 'static,
{
    let idle_timeout = websocket_idle_timeout(provider);
    let continuation = provider
        .websocket_session
        .lock()
        .await
        .prepare_continuation(provider, token, &body, stable_client_conversation_id);
    info!(
        transport = "websocket",
        continuation_key_present = continuation.key.is_some(),
        continuation_used = continuation.used,
        continuation_disabled_reason = continuation.disabled_reason.as_str(),
        continuation_delta_items = continuation
            .send_body
            .get("input")
            .and_then(|value| value.as_array())
            .map(Vec::len)
            .unwrap_or(0),
        continuation_cached_input_items = continuation.full_input.len(),
        continuation_schema_version = CHATGPT_CONTINUATION_SCHEMA_VERSION,
        "ChatGPT websocket continuation decision"
    );

    let (mut stream, reused) = match checkout_connection(provider, token).await {
        Ok((stream, reused)) => (stream, reused),
        Err(error) => {
            provider
                .websocket_session
                .lock()
                .await
                .fail_continuation(&continuation);
            return Err(error);
        }
    };
    let request_text = match response_create_request_text(continuation.send_body.clone()) {
        Ok(request_text) => request_text,
        Err(error) => {
            provider
                .websocket_session
                .lock()
                .await
                .fail_continuation(&continuation);
            return Err(error);
        }
    };
    if let Err(error) = send_websocket_request(&mut stream, request_text, idle_timeout).await {
        provider
            .websocket_session
            .lock()
            .await
            .fail_continuation(&continuation);
        return Err(ChatGptWebSocketStartError::new(error, true));
    }

    let first_event = match read_next_json_event(&mut stream, idle_timeout, true).await {
        Ok(event) => event,
        Err(error) => {
            provider
                .websocket_session
                .lock()
                .await
                .fail_continuation(&continuation);
            return Err(error);
        }
    };
    let first_event_terminal = is_terminal_response_event(&first_event);
    let (tx_event, rx_event) = mpsc::channel::<Result<Value, ProviderError>>(1600);
    if tx_event.send(Ok(first_event.clone())).await.is_err() {
        provider
            .websocket_session
            .lock()
            .await
            .fail_continuation(&continuation);
        return Err(ChatGptWebSocketStartError::new(
            response_consumer_dropped_error(),
            false,
        ));
    }

    let websocket_session = provider.websocket_session.clone();
    let (abort_tx, mut abort_rx) = watch::channel(false);
    tokio::spawn(async move {
        if first_event_terminal {
            websocket_session
                .lock()
                .await
                .complete_continuation(&continuation, &first_event);
            store_completed_connection(websocket_session, stream).await;
            return;
        }

        loop {
            tokio::select! {
                _ = abort_rx.changed() => {
                    websocket_session
                        .lock()
                        .await
                        .fail_continuation(&continuation);
                    close_or_release_websocket(stream).await;
                    return;
                }
                result = read_next_json_event(&mut stream, idle_timeout, false) => {
                    match result {
                        Ok(event) => {
                            let terminal = is_terminal_response_event(&event);
                            if tx_event.send(Ok(event.clone())).await.is_err() {
                                websocket_session
                                    .lock()
                                    .await
                                    .fail_continuation(&continuation);
                                close_or_release_websocket(stream).await;
                                return;
                            }
                            if terminal {
                                websocket_session
                                    .lock()
                                    .await
                                    .complete_continuation(&continuation, &event);
                                store_completed_connection(websocket_session, stream).await;
                                return;
                            }
                        }
                        Err(error) => {
                            websocket_session
                                .lock()
                                .await
                                .fail_continuation(&continuation);
                            let _ = tx_event.send(Err(error.error)).await;
                            return;
                        }
                    }
                }
            }
        }
    });

    let stream = crate::responses::stream_responses_json_events_with_marker_mode_and_observer(
        rx_event,
        marker_mode,
        on_event,
    );
    Ok((Box::pin(AbortOnDropStream::new(stream, abort_tx)), reused))
}

async fn checkout_connection(
    provider: &ChatGptProvider,
    token: &ChatGptToken,
) -> Result<(ChatGptWsStream, bool), ChatGptWebSocketStartError> {
    if let Some(stream) = provider.websocket_session.lock().await.take_fresh() {
        return Ok((stream, true));
    }

    let url = websocket_url(provider)?;
    let headers = websocket_headers(provider, token)?;
    let stream = connect_websocket(provider, url, headers).await?;
    Ok((stream, false))
}

async fn store_completed_connection(
    websocket_session: std::sync::Arc<tokio::sync::Mutex<ChatGptWebSocketSession>>,
    stream: ChatGptWsStream,
) {
    let stored = websocket_session.lock().await.store_if_empty(stream);
    info!(
        transport = "websocket",
        websocket_reused = false,
        websocket_cached = stored,
        "ChatGPT websocket response stream completed"
    );
}

fn websocket_url(provider: &ChatGptProvider) -> Result<Url, ChatGptWebSocketStartError> {
    let mut url = Url::parse(&provider.endpoint).map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(format!(
                "invalid ChatGPT websocket endpoint {}: {error}",
                provider.endpoint
            )),
            false,
        )
    })?;

    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(ChatGptWebSocketStartError::new(
                ProviderError::InvalidRequest(format!(
                    "unsupported ChatGPT websocket endpoint scheme: {other}"
                )),
                false,
            ));
        }
    };
    url.set_scheme(scheme).map_err(|_| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(format!(
                "failed to convert ChatGPT endpoint to websocket URL: {}",
                provider.endpoint
            )),
            false,
        )
    })?;

    if !provider.runtime.request.query_params.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in &provider.runtime.request.query_params {
            pairs.append_pair(name, value);
        }
    }

    Ok(url)
}

fn websocket_headers(
    provider: &ChatGptProvider,
    token: &ChatGptToken,
) -> Result<HeaderMap, ChatGptWebSocketStartError> {
    let mut headers = HeaderMap::new();
    let authorization = format!("Bearer {}", token.access_token);
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&authorization).map_err(invalid_header_error("authorization"))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(USER_AGENT, provider.request_headers.user_agent.clone());
    headers.insert(
        HeaderName::from_static("originator"),
        provider.request_headers.originator.clone(),
    );
    headers.insert(
        HeaderName::from_static("x-client-request-id"),
        HeaderValue::from_str(&provider.thread_id)
            .map_err(invalid_header_error("x-client-request-id"))?,
    );
    headers.insert(
        HeaderName::from_static("session-id"),
        HeaderValue::from_str(&provider.session_id).map_err(invalid_header_error("session-id"))?,
    );
    headers.insert(
        HeaderName::from_static("thread-id"),
        HeaderValue::from_str(&provider.thread_id).map_err(invalid_header_error("thread-id"))?,
    );
    headers.insert(
        HeaderName::from_static("x-codex-window-id"),
        HeaderValue::from_str(&provider.window_id)
            .map_err(invalid_header_error("x-codex-window-id"))?,
    );
    headers.insert(
        HeaderName::from_static(OPENAI_BETA_HEADER),
        HeaderValue::from_static(RESPONSES_WEBSOCKETS_BETA),
    );

    if let Some(account_id) = token.account_id.as_deref() {
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_str(account_id)
                .map_err(invalid_header_error("chatgpt-account-id"))?,
        );
    }

    for (name, value) in &provider.runtime.request.extra_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            ChatGptWebSocketStartError::new(
                ProviderError::InvalidRequest(format!(
                    "invalid provider runtime header {name}: {error}"
                )),
                false,
            )
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            ChatGptWebSocketStartError::new(
                ProviderError::InvalidRequest(format!(
                    "invalid provider runtime header {name}: {error}"
                )),
                false,
            )
        })?;
        headers.insert(header_name, header_value);
    }

    Ok(headers)
}

fn invalid_header_error(
    name: &'static str,
) -> impl FnOnce(reqwest::header::InvalidHeaderValue) -> ChatGptWebSocketStartError {
    move |error| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(format!(
                "invalid ChatGPT websocket header {name}: {error}"
            )),
            false,
        )
    }
}

async fn connect_websocket(
    provider: &ChatGptProvider,
    url: Url,
    headers: HeaderMap,
) -> Result<ChatGptWsStream, ChatGptWebSocketStartError> {
    let mut request = url.as_str().into_client_request().map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(format!(
                "failed to build ChatGPT websocket request: {error}"
            )),
            false,
        )
    })?;
    request.headers_mut().extend(headers);
    let connector = websocket_tls_connector(&provider.extra_ca_certs)?;

    let (stream, response) = tokio::time::timeout(CHATGPT_WEBSOCKET_CONNECT_TIMEOUT, async {
        if let Some(proxy) = provider.proxy.as_deref() {
            let socket = connect_proxy_tunnel(proxy, &url).await?;
            client_async_tls_with_config(request, socket, None, connector)
                .await
                .map_err(map_websocket_connect_error)
        } else {
            connect_async_tls_with_config(request, None, false, connector)
                .await
                .map_err(map_websocket_connect_error)
        }
    })
    .await
    .map_err(|_| ChatGptWebSocketStartError::new(ProviderError::Timeout, true))??;

    info!(
        transport = "websocket",
        status = response.status().as_u16(),
        endpoint = %url,
        proxy = provider.proxy.is_some(),
        extra_ca_certs = provider.extra_ca_certs.len(),
        "ChatGPT websocket connected"
    );
    Ok(stream)
}

fn websocket_tls_connector(
    extra_ca_certs: &[String],
) -> Result<Option<Connector>, ChatGptWebSocketStartError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    for path in extra_ca_certs {
        let pem = fs::read(path).map_err(|error| {
            ChatGptWebSocketStartError::new(
                ProviderError::Network(format!("failed to read extra CA cert {path}: {error}")),
                false,
            )
        })?;
        let certs = rustls_pemfile::certs(&mut pem.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                ChatGptWebSocketStartError::new(
                    ProviderError::Network(format!(
                        "failed to parse extra CA cert {path}: {error}"
                    )),
                    false,
                )
            })?;
        let (valid, _invalid) = roots.add_parsable_certificates(certs);
        if valid == 0 {
            return Err(ChatGptWebSocketStartError::new(
                ProviderError::Network(format!(
                    "extra CA cert {path} did not contain valid certificates"
                )),
                false,
            ));
        }
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(Connector::Rustls(Arc::new(config))))
}

async fn connect_proxy_tunnel(
    proxy: &str,
    target_url: &Url,
) -> Result<TcpStream, ChatGptWebSocketStartError> {
    let proxy_url = Url::parse(proxy).map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(format!("invalid ChatGPT websocket proxy URL: {error}")),
            true,
        )
    })?;
    if proxy_url.scheme() != "http" {
        return Err(ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(
                "ChatGPT websocket transport currently supports HTTP proxy URLs only".to_string(),
            ),
            true,
        ));
    }

    let proxy_host = proxy_url.host_str().ok_or_else(|| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(
                "ChatGPT websocket proxy URL is missing host".to_string(),
            ),
            true,
        )
    })?;
    let proxy_port = proxy_url.port_or_known_default().ok_or_else(|| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(
                "ChatGPT websocket proxy URL is missing port".to_string(),
            ),
            true,
        )
    })?;
    let target_host = target_url.host_str().ok_or_else(|| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(
                "ChatGPT websocket target URL is missing host".to_string(),
            ),
            false,
        )
    })?;
    let target_port = target_url.port_or_known_default().ok_or_else(|| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(
                "ChatGPT websocket target URL is missing port".to_string(),
            ),
            false,
        )
    })?;
    let target_authority = format!("{target_host}:{target_port}");
    let mut socket = tokio::time::timeout(
        CHATGPT_WEBSOCKET_CONNECT_TIMEOUT,
        TcpStream::connect((proxy_host, proxy_port)),
    )
    .await
    .map_err(|_| ChatGptWebSocketStartError::new(ProviderError::Timeout, true))?
    .map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::Network(format!(
                "failed to connect ChatGPT websocket proxy: {error}"
            )),
            true,
        )
    })?;

    let mut request =
        format!("CONNECT {target_authority} HTTP/1.1\r\nHost: {target_authority}\r\n");
    if !proxy_url.username().is_empty() {
        let credentials = format!(
            "{}:{}",
            proxy_url.username(),
            proxy_url.password().unwrap_or_default()
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
        request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    request.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");

    tokio::time::timeout(
        CHATGPT_WEBSOCKET_CONNECT_TIMEOUT,
        socket.write_all(request.as_bytes()),
    )
    .await
    .map_err(|_| ChatGptWebSocketStartError::new(ProviderError::Timeout, true))?
    .map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::Network(format!(
                "failed to write ChatGPT websocket proxy CONNECT: {error}"
            )),
            true,
        )
    })?;

    let response = read_proxy_connect_response(&mut socket).await?;
    let status = parse_proxy_connect_status(&response).ok_or_else(|| {
        ChatGptWebSocketStartError::new(
            ProviderError::Network("invalid ChatGPT websocket proxy CONNECT response".to_string()),
            true,
        )
    })?;
    if !(200..300).contains(&status) {
        return Err(ChatGptWebSocketStartError::new(
            ProviderError::Network(format!(
                "ChatGPT websocket proxy CONNECT failed with HTTP {status}"
            )),
            true,
        ));
    }

    Ok(socket)
}

async fn read_proxy_connect_response(
    socket: &mut TcpStream,
) -> Result<Vec<u8>, ChatGptWebSocketStartError> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 512];
    loop {
        let read =
            tokio::time::timeout(CHATGPT_WEBSOCKET_CONNECT_TIMEOUT, socket.read(&mut buffer))
                .await
                .map_err(|_| ChatGptWebSocketStartError::new(ProviderError::Timeout, true))?
                .map_err(|error| {
                    ChatGptWebSocketStartError::new(
                        ProviderError::Network(format!(
                            "failed to read ChatGPT websocket proxy CONNECT response: {error}"
                        )),
                        true,
                    )
                })?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if response.len() > 16 * 1024 {
            return Err(ChatGptWebSocketStartError::new(
                ProviderError::Network(
                    "ChatGPT websocket proxy CONNECT response too large".to_string(),
                ),
                true,
            ));
        }
    }
    Ok(response)
}

fn parse_proxy_connect_status(response: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(response).ok()?;
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let _http_version = parts.next()?;
    parts.next()?.parse().ok()
}

fn map_websocket_connect_error(error: WsError) -> ChatGptWebSocketStartError {
    match error {
        WsError::Http(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let headers = response.headers().clone();
            let body = response
                .body()
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
                .unwrap_or_default();
            let fallback_allowed = websocket_http_fallback_allowed(status, &body);
            ChatGptWebSocketStartError::new(
                map_chatgpt_error_status_body_with_headers(status, &headers, body),
                fallback_allowed,
            )
        }
        other => ChatGptWebSocketStartError::new(
            ProviderError::Network(format!("ChatGPT websocket connect failed: {other}")),
            true,
        ),
    }
}

fn response_create_request_text(mut body: Value) -> Result<String, ChatGptWebSocketStartError> {
    let object = body.as_object_mut().ok_or_else(|| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(
                "ChatGPT websocket request body must be an object".to_string(),
            ),
            false,
        )
    })?;
    object.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    serde_json::to_string(&body).map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::InvalidRequest(format!(
                "failed to encode ChatGPT websocket request: {error}"
            )),
            false,
        )
    })
}

async fn send_websocket_request(
    stream: &mut ChatGptWsStream,
    request_text: String,
    idle_timeout: Duration,
) -> Result<(), ProviderError> {
    tokio::time::timeout(
        idle_timeout,
        stream.send(Message::Text(request_text.into())),
    )
    .await
    .map_err(|_| ProviderError::Timeout)?
    .map_err(|error| {
        ProviderError::Network(format!("failed to send ChatGPT websocket request: {error}"))
    })
}

async fn read_next_json_event(
    stream: &mut ChatGptWsStream,
    idle_timeout: Duration,
    before_first_event: bool,
) -> Result<Value, ChatGptWebSocketStartError> {
    loop {
        let message = tokio::time::timeout(idle_timeout, stream.next())
            .await
            .map_err(|_| {
                ChatGptWebSocketStartError::new(ProviderError::Timeout, before_first_event)
            })?;
        let message = match message {
            Some(Ok(message)) => message,
            Some(Err(error)) => {
                let error =
                    ProviderError::Network(format!("ChatGPT websocket read failed: {error}"));
                return Err(ChatGptWebSocketStartError::new(error, before_first_event));
            }
            None => {
                let error = ProviderError::Network(
                    "ChatGPT websocket closed before response.completed".to_string(),
                );
                return Err(ChatGptWebSocketStartError::new(error, before_first_event));
            }
        };

        match message {
            Message::Text(text) => return parse_text_event(&text, before_first_event),
            Message::Binary(_) => {
                return Err(ChatGptWebSocketStartError::new(
                    ProviderError::UpstreamError {
                        status: 200,
                        body: "unexpected binary ChatGPT websocket event".to_string(),
                    },
                    false,
                ));
            }
            Message::Close(frame) => {
                let reason = frame
                    .as_ref()
                    .map(|frame| frame.reason.to_string())
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or_else(|| "no close reason".to_string());
                return Err(ChatGptWebSocketStartError::new(
                    ProviderError::Network(format!(
                        "ChatGPT websocket closed before response.completed: {reason}"
                    )),
                    before_first_event,
                ));
            }
            Message::Ping(payload) => {
                stream.send(Message::Pong(payload)).await.map_err(|error| {
                    ChatGptWebSocketStartError::new(
                        ProviderError::Network(format!(
                            "failed to send ChatGPT websocket pong: {error}"
                        )),
                        before_first_event,
                    )
                })?;
            }
            Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

fn parse_text_event(
    text: &str,
    before_first_event: bool,
) -> Result<Value, ChatGptWebSocketStartError> {
    let value = serde_json::from_str::<Value>(text).map_err(|error| {
        ChatGptWebSocketStartError::new(
            ProviderError::UpstreamError {
                status: 200,
                body: format!("invalid ChatGPT websocket event JSON: {error}"),
            },
            false,
        )
    })?;

    if value.get("type").and_then(|value| value.as_str()) == Some("error") {
        return Err(ChatGptWebSocketStartError::new(
            map_wrapped_websocket_error_event(&value, text),
            false,
        ));
    }

    if before_first_event {
        info!(
            transport = "websocket",
            event_type = value
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            "ChatGPT websocket first upstream event received"
        );
    }

    Ok(value)
}

fn websocket_http_fallback_allowed(status: StatusCode, body: &str) -> bool {
    if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
    ) {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    !lower.contains("usage_limit") && !lower.contains("rate_limit")
}

#[derive(Debug, Deserialize)]
struct WrappedWebSocketErrorEvent {
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    error: Option<WrappedWebSocketError>,
    #[serde(default)]
    headers: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct WrappedWebSocketError {
    code: Option<String>,
    message: Option<String>,
}

fn map_wrapped_websocket_error_event(value: &Value, original_text: &str) -> ProviderError {
    let parsed = serde_json::from_value::<WrappedWebSocketErrorEvent>(value.clone()).ok();
    let status = parsed
        .as_ref()
        .and_then(|event| event.status)
        .unwrap_or(200);
    let message = parsed
        .as_ref()
        .and_then(|event| event.error.as_ref())
        .and_then(|error| error.message.clone())
        .unwrap_or_else(|| original_text.to_string());
    let code = parsed
        .as_ref()
        .and_then(|event| event.error.as_ref())
        .and_then(|error| error.code.as_deref());

    if code == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE) {
        return ProviderError::Overloaded {
            message,
            retry_after: None,
        };
    }

    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = parsed
        .and_then(|event| event.headers)
        .map(json_headers_to_header_map)
        .unwrap_or_default();
    map_chatgpt_error_status_body_with_headers(status, &headers, original_text.to_string())
}

fn json_headers_to_header_map(headers: serde_json::Map<String, Value>) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    for (name, value) in headers {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Some(header_value) = json_header_value(value) else {
            continue;
        };
        mapped.insert(header_name, header_value);
    }
    mapped
}

fn json_header_value(value: Value) -> Option<HeaderValue> {
    let value = match value {
        Value::String(value) => value,
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => return None,
    };
    HeaderValue::from_str(&value).ok()
}

fn is_terminal_response_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(|value| value.as_str()),
        Some("response.completed" | "response.incomplete" | "response.failed")
    )
}

fn websocket_idle_timeout(provider: &ChatGptProvider) -> Duration {
    provider
        .runtime
        .request
        .stream_idle_timeout_seconds
        .map(Duration::from_secs)
        .unwrap_or(CHATGPT_WEBSOCKET_IDLE_TIMEOUT)
}

async fn close_or_release_websocket(mut stream: ChatGptWsStream) {
    let _ = tokio::time::timeout(Duration::from_secs(1), stream.close(None)).await;
}

fn response_consumer_dropped_error() -> ProviderError {
    ProviderError::Network("ChatGPT websocket response consumer dropped".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_create_request_text_wraps_body_with_type() {
        let text = response_create_request_text(json!({
            "model": "gpt-5.3-codex",
            "input": [],
            "stream": true
        }))
        .expect("request text");
        let value: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(value["type"], "response.create");
        assert_eq!(value["model"], "gpt-5.3-codex");
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn continuation_canonical_body_ignores_only_input_previous_response_and_type() {
        let first = canonical_request_body(&json!({
            "type": "response.create",
            "previous_response_id": "resp-1",
            "input": [{"role": "user", "content": "hi"}],
            "model": "gpt-5.3-codex",
            "text": {"verbosity": "high"},
            "tools": [{"name": "Read", "type": "function"}]
        }));
        let second = canonical_request_body(&json!({
            "tools": [{"type": "function", "name": "Read"}],
            "text": {"verbosity": "high"},
            "model": "gpt-5.3-codex",
            "input": [{"role": "user", "content": "different"}]
        }));
        let changed = canonical_request_body(&json!({
            "model": "gpt-5.3-codex",
            "text": {"verbosity": "low"},
            "tools": [{"name": "Read", "type": "function"}],
            "input": [{"role": "user", "content": "hi"}]
        }));

        assert_eq!(first, second);
        assert_ne!(first, changed);
    }

    #[test]
    fn continuation_extracts_supported_assistant_output_prefix_items() {
        let event = json!({
            "type": "response.completed",
            "response": {
                "id": "resp-1",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "Read",
                        "arguments": "{\"file\":\"a\"}"
                    }
                ]
            }
        });

        let items = terminal_assistant_output_items(&event).expect("supported output items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], json!({"role": "assistant", "content": "hello"}));
        assert_eq!(
            items[1],
            json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "Read",
                "arguments": "{\"file\":\"a\"}"
            })
        );
    }

    #[test]
    fn continuation_rejects_unsupported_assistant_output_prefix_items() {
        let event = json!({
            "type": "response.completed",
            "response": {
                "id": "resp-1",
                "status": "completed",
                "output": [{"type": "custom_tool_call", "call_id": "call-1"}]
            }
        });

        assert!(terminal_assistant_output_items(&event).is_none());
    }

    #[test]
    fn continuation_prunes_expired_entries() {
        let mut session = ChatGptWebSocketSession::new();
        let key = ContinuationKey {
            provider_id: "chatgpt".to_string(),
            account_id: "account".to_string(),
            model: "gpt-5.3-codex".to_string(),
            stable_client_conversation_id: "conversation".to_string(),
            schema_version: CHATGPT_CONTINUATION_SCHEMA_VERSION,
        };
        session.continuations.insert(
            key,
            CachedContinuation {
                canonical_body: json!({"model": "gpt-5.3-codex"}),
                full_input: vec![json!({"role": "user", "content": "hi"})],
                assistant_output_items: Vec::new(),
                response_id: "resp-1".to_string(),
                updated_at: Instant::now()
                    - CHATGPT_WEBSOCKET_SESSION_IDLE_TTL
                    - Duration::from_secs(1),
            },
        );

        session.prune_expired_continuations();

        assert!(session.continuations.is_empty());
    }

    #[test]
    fn websocket_http_fallback_excludes_auth_and_rate_limits() {
        assert!(websocket_http_fallback_allowed(
            StatusCode::UPGRADE_REQUIRED,
            ""
        ));
        assert!(!websocket_http_fallback_allowed(
            StatusCode::UNAUTHORIZED,
            ""
        ));
        assert!(!websocket_http_fallback_allowed(
            StatusCode::TOO_MANY_REQUESTS,
            ""
        ));
        assert!(!websocket_http_fallback_allowed(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"usage_limit_reached"}}"#,
        ));
    }
}
