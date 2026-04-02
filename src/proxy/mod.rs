use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::Sse;
use axum::response::sse::Event;
use axum::routing::get;
use axum::routing::post;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::commands;
use crate::context::AppContext;
use crate::store;

pub mod anthropic;
pub mod backend;
pub mod openai;
pub mod runtime;
pub mod scheduler;

use runtime::RuntimeUsage;
use scheduler::Scheduler;
use scheduler::SchedulerLease;

#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    pub listen: Option<String>,
    pub cwd: Option<PathBuf>,
    pub api_key: Option<String>,
    pub default_model: Option<String>,
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
    pub usage_refresh_interval: Option<u64>,
    pub max_concurrent_requests: Option<usize>,
    pub max_inflight_per_account: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolKind {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct ProxyTurnMessage {
    pub role: ProxyRole,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ProxyTurnRequest {
    pub protocol: ProtocolKind,
    pub model_alias: String,
    pub resolved_model: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<ProxyTurnMessage>,
    pub stream: bool,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ResolvedProxyConfig {
    pub listen: String,
    pub api_key: String,
    pub default_cwd: PathBuf,
    pub default_model: String,
    pub sandbox: String,
    pub approval_policy: String,
    pub usage_refresh_interval_seconds: u64,
    pub max_concurrent_requests: usize,
    pub max_inflight_per_account: usize,
    pub model_aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ProxyExecutionEnvelope {
    pub response_id: String,
    pub created: i64,
    pub model_alias: String,
    pub text: String,
    pub usage: RuntimeUsage,
}

#[derive(Debug, Clone)]
pub struct ProtocolError {
    pub status: StatusCode,
    pub message: String,
}

struct ServiceState {
    context: AppContext,
    config: ResolvedProxyConfig,
    scheduler: Arc<Scheduler>,
    backend: Arc<backend::BackendManager>,
    refresh_lock: Mutex<()>,
}

struct StreamReady {
    envelope: ProxyExecutionEnvelope,
    account_id: String,
    account_label: String,
    first_delta: String,
    execution: Option<backend::BackendExecution>,
    completion: Option<backend::BackendSuccess>,
    _lease: SchedulerLease,
}

#[derive(Debug, serde::Serialize)]
struct HealthResponse {
    ok: bool,
    account_count: usize,
}

#[derive(Debug, serde::Serialize)]
struct AdminAccountResponse {
    id: String,
    label: String,
    account_id: String,
    status: String,
    inflight: usize,
    cooldown_until: Option<i64>,
    current: bool,
    five_hour_used_percent: Option<f64>,
    one_week_used_percent: Option<f64>,
    usage_error: Option<String>,
}

impl ProtocolError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

pub async fn serve(context: AppContext, options: ServeOptions) -> anyhow::Result<()> {
    serve_with_shutdown(context, options, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

pub async fn serve_with_shutdown<F>(
    context: AppContext,
    options: ServeOptions,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let config = resolve_proxy_config(&context, options)?;
    let summaries = commands::refresh_accounts(&context, None).await?;
    let store_value = store::load_store(&context)?;
    let scheduler = Scheduler::new(
        &store_value,
        &summaries,
        config.max_concurrent_requests,
        config.max_inflight_per_account,
    );

    let state = Arc::new(ServiceState {
        context: context.clone(),
        config: config.clone(),
        scheduler,
        backend: backend::BackendManager::new(context.clone(), config.clone()),
        refresh_lock: Mutex::new(()),
    });
    let refresh_state = Arc::clone(&state);
    let refresh_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            refresh_state.config.usage_refresh_interval_seconds,
        ));
        loop {
            interval.tick().await;
            let _ = refresh_state.refresh_accounts(None).await;
        }
    });

    let app = build_router(state);
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind proxy listener {}", config.listen))?;
    println!("codex-pool proxy listening on http://{}", config.listen);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    refresh_task.abort();
    result.context("proxy server exited unexpectedly")
}

fn build_router(state: Arc<ServiceState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(openai_chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/models", get(openai_models))
        .route("/healthz", get(healthz))
        .route("/admin/accounts", get(admin_accounts))
        .with_state(state)
}

fn resolve_proxy_config(
    context: &AppContext,
    options: ServeOptions,
) -> anyhow::Result<ResolvedProxyConfig> {
    let mut proxy = context.load_config()?.proxy;
    if let Some(listen) = options.listen {
        proxy.listen = listen;
    }
    if let Some(cwd) = options.cwd {
        proxy.default_cwd = cwd.display().to_string();
    }
    if let Some(api_key) = options.api_key {
        proxy.api_key = api_key;
    }
    if let Some(default_model) = options.default_model {
        proxy.default_model = default_model;
    }
    if let Some(sandbox) = options.sandbox {
        proxy.sandbox = sandbox;
    }
    if let Some(approval_policy) = options.approval_policy {
        proxy.approval_policy = approval_policy;
    }
    if let Some(interval) = options.usage_refresh_interval {
        proxy.usage_refresh_interval_seconds = interval;
    }
    if let Some(max_concurrent_requests) = options.max_concurrent_requests {
        proxy.max_concurrent_requests = max_concurrent_requests;
    }
    if let Some(max_inflight_per_account) = options.max_inflight_per_account {
        proxy.max_inflight_per_account = max_inflight_per_account;
    }
    proxy.normalize(
        &std::env::current_dir()
            .unwrap_or_else(|_| context.paths.home_dir.clone())
            .display()
            .to_string(),
    );

    let default_cwd = PathBuf::from(&proxy.default_cwd);
    Ok(ResolvedProxyConfig {
        listen: proxy.listen,
        api_key: proxy.api_key,
        default_cwd,
        default_model: proxy.default_model,
        sandbox: proxy.sandbox,
        approval_policy: proxy.approval_policy,
        usage_refresh_interval_seconds: proxy.usage_refresh_interval_seconds,
        max_concurrent_requests: proxy.max_concurrent_requests,
        max_inflight_per_account: proxy.max_inflight_per_account,
        model_aliases: proxy.model_aliases,
    })
}

impl ServiceState {
    async fn refresh_accounts(&self, target_ref: Option<&str>) -> anyhow::Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let summaries = commands::refresh_accounts(&self.context, target_ref).await?;
        let store_value = store::load_store(&self.context)?;
        self.scheduler.replace_accounts(&store_value, &summaries)?;
        Ok(())
    }

    async fn execute(
        &self,
        turn: &ProxyTurnRequest,
    ) -> Result<(ProxyExecutionEnvelope, String, String), ProtocolError> {
        let max_attempts = self
            .scheduler
            .snapshots()
            .map(|items| items.len())
            .unwrap_or(1)
            .max(1);
        let mut last_error = None;

        for _ in 0..max_attempts {
            let lease = self.scheduler.acquire().await.map_err(|error| {
                ProtocolError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            })?;
            let account_id = lease.account.account_id.clone();
            let account_label = lease.account.label.clone();
            let mut execution = match self.backend.start_turn(&lease.account, turn).await {
                Ok(execution) => execution,
                Err(error) if error.kind == backend::BackendErrorKind::Retriable => {
                    self.backend.invalidate(&account_id).await;
                    let _ = self.scheduler.cooldown_account(&account_id, None);
                    let _ = self.refresh_accounts(Some(&account_id)).await;
                    last_error = Some(error.message);
                    continue;
                }
                Err(error) => {
                    return Err(ProtocolError::new(StatusCode::BAD_GATEWAY, error.message));
                }
            };

            while let Some(event) = execution.next().await {
                match event {
                    Ok(backend::BackendEvent::TextDelta(_)) => {}
                    Ok(backend::BackendEvent::Completed(success)) => {
                        let envelope = match turn.protocol {
                            ProtocolKind::OpenAi => openai::new_execution_envelope(
                                &turn.model_alias,
                                &success.text,
                                success.usage,
                            ),
                            ProtocolKind::Anthropic => anthropic::new_execution_envelope(
                                &turn.model_alias,
                                &success.text,
                                success.usage,
                            ),
                        };
                        return Ok((envelope, account_id, account_label));
                    }
                    Err(error) if error.kind == backend::BackendErrorKind::Retriable => {
                        self.backend.invalidate(&account_id).await;
                        let _ = self.scheduler.cooldown_account(&account_id, None);
                        let _ = self.refresh_accounts(Some(&account_id)).await;
                        last_error = Some(error.message);
                        break;
                    }
                    Err(error) => {
                        return Err(ProtocolError::new(StatusCode::BAD_GATEWAY, error.message));
                    }
                }
            }
        }

        Err(ProtocolError::new(
            StatusCode::BAD_GATEWAY,
            last_error.unwrap_or_else(|| "all proxy accounts failed".to_string()),
        ))
    }

    async fn prepare_stream(&self, turn: &ProxyTurnRequest) -> Result<StreamReady, ProtocolError> {
        let max_attempts = self
            .scheduler
            .snapshots()
            .map(|items| items.len())
            .unwrap_or(1)
            .max(1);
        let mut last_error = None;

        for _ in 0..max_attempts {
            let lease = self.scheduler.acquire().await.map_err(|error| {
                ProtocolError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            })?;
            let account_id = lease.account.account_id.clone();
            let account_label = lease.account.label.clone();
            let mut execution = match self.backend.start_turn(&lease.account, turn).await {
                Ok(execution) => execution,
                Err(error) if error.kind == backend::BackendErrorKind::Retriable => {
                    self.backend.invalidate(&account_id).await;
                    let _ = self.scheduler.cooldown_account(&account_id, None);
                    let _ = self.refresh_accounts(Some(&account_id)).await;
                    last_error = Some(error.message);
                    continue;
                }
                Err(error) => {
                    return Err(ProtocolError::new(StatusCode::BAD_GATEWAY, error.message));
                }
            };

            let mut envelope = match turn.protocol {
                ProtocolKind::OpenAi => {
                    openai::new_execution_envelope(&turn.model_alias, "", RuntimeUsage::default())
                }
                ProtocolKind::Anthropic => anthropic::new_execution_envelope(
                    &turn.model_alias,
                    "",
                    RuntimeUsage::default(),
                ),
            };

            while let Some(event) = execution.next().await {
                match event {
                    Ok(backend::BackendEvent::TextDelta(delta)) if !delta.is_empty() => {
                        envelope.text.push_str(&delta);
                        return Ok(StreamReady {
                            envelope,
                            account_id,
                            account_label,
                            first_delta: delta,
                            execution: Some(execution),
                            completion: None,
                            _lease: lease,
                        });
                    }
                    Ok(backend::BackendEvent::TextDelta(_)) => {}
                    Ok(backend::BackendEvent::Completed(success)) => {
                        envelope.text = success.text.clone();
                        envelope.usage = success.usage.clone();
                        return Ok(StreamReady {
                            envelope,
                            account_id,
                            account_label,
                            first_delta: success.text.clone(),
                            execution: None,
                            completion: Some(success),
                            _lease: lease,
                        });
                    }
                    Err(error) if error.kind == backend::BackendErrorKind::Retriable => {
                        self.backend.invalidate(&account_id).await;
                        let _ = self.scheduler.cooldown_account(&account_id, None);
                        let _ = self.refresh_accounts(Some(&account_id)).await;
                        last_error = Some(error.message);
                        break;
                    }
                    Err(error) => {
                        return Err(ProtocolError::new(StatusCode::BAD_GATEWAY, error.message));
                    }
                }
            }
        }

        Err(ProtocolError::new(
            StatusCode::BAD_GATEWAY,
            last_error.unwrap_or_else(|| "all proxy accounts failed".to_string()),
        ))
    }
}

async fn openai_chat_completions(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Json(request): Json<openai::ChatCompletionsRequest>,
) -> Response {
    if let Err(error) = authorize_openai(&headers, &state.config.api_key) {
        return openai_error_response(error);
    }
    let turn = match openai::into_turn_request(request, &state.config) {
        Ok(turn) => turn,
        Err(error) => return openai_error_response(error),
    };
    if turn.stream {
        return match state.prepare_stream(&turn).await {
            Ok(ready) => {
                let account_id = ready.account_id.clone();
                let account_label = ready.account_label.clone();
                let stream = stream! {
                    let mut ready = ready;
                    yield Ok::<Event, Infallible>(Event::default().data(openai::stream_role_chunk(&ready.envelope)));
                    if !ready.first_delta.is_empty() {
                        yield Ok::<Event, Infallible>(Event::default().data(openai::stream_text_chunk(&ready.envelope, &ready.first_delta)));
                    }

                    if let Some(success) = ready.completion.take() {
                        ready.envelope.text = success.text;
                        ready.envelope.usage = success.usage;
                    } else if let Some(mut execution) = ready.execution.take() {
                        while let Some(event) = execution.next().await {
                            match event {
                                Ok(backend::BackendEvent::TextDelta(delta)) => {
                                    if !delta.is_empty() {
                                        ready.envelope.text.push_str(&delta);
                                        yield Ok::<Event, Infallible>(Event::default().data(openai::stream_text_chunk(&ready.envelope, &delta)));
                                    }
                                }
                                Ok(backend::BackendEvent::Completed(success)) => {
                                    ready.envelope.text = success.text;
                                    ready.envelope.usage = success.usage;
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                    }

                    yield Ok::<Event, Infallible>(Event::default().data(openai::stream_stop_chunk(&ready.envelope)));
                    yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
                };
                with_account_headers(
                    Sse::new(stream).into_response(),
                    &account_id,
                    &account_label,
                )
            }
            Err(error) => openai_error_response(error),
        };
    }

    match state.execute(&turn).await {
        Ok((envelope, account_id, account_label)) => with_account_headers(
            Json(openai::completion_response(&envelope)).into_response(),
            &account_id,
            &account_label,
        ),
        Err(error) => openai_error_response(error),
    }
}

async fn anthropic_messages(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Json(request): Json<anthropic::MessagesRequest>,
) -> Response {
    if let Err(error) = authorize_anthropic(&headers, &state.config.api_key) {
        return anthropic_error_response(error);
    }
    let turn = match anthropic::into_turn_request(request, &state.config) {
        Ok(turn) => turn,
        Err(error) => return anthropic_error_response(error),
    };
    if turn.stream {
        return match state.prepare_stream(&turn).await {
            Ok(ready) => {
                let account_id = ready.account_id.clone();
                let account_label = ready.account_label.clone();
                let stream = stream! {
                    let mut ready = ready;
                    for (name, payload) in anthropic::stream_start_events(&ready.envelope) {
                        let mut event = Event::default().data(payload);
                        if let Some(name) = name {
                            event = event.event(name);
                        }
                        yield Ok::<Event, Infallible>(event);
                    }
                    if !ready.first_delta.is_empty() {
                        let (name, payload) = anthropic::stream_text_event(&ready.first_delta);
                        let mut event = Event::default().data(payload);
                        if let Some(name) = name {
                            event = event.event(name);
                        }
                        yield Ok::<Event, Infallible>(event);
                    }

                    if let Some(success) = ready.completion.take() {
                        ready.envelope.text = success.text;
                        ready.envelope.usage = success.usage;
                    } else if let Some(mut execution) = ready.execution.take() {
                        while let Some(event) = execution.next().await {
                            match event {
                                Ok(backend::BackendEvent::TextDelta(delta)) => {
                                    if !delta.is_empty() {
                                        ready.envelope.text.push_str(&delta);
                                        let (name, payload) = anthropic::stream_text_event(&delta);
                                        let mut event = Event::default().data(payload);
                                        if let Some(name) = name {
                                            event = event.event(name);
                                        }
                                        yield Ok::<Event, Infallible>(event);
                                    }
                                }
                                Ok(backend::BackendEvent::Completed(success)) => {
                                    ready.envelope.text = success.text;
                                    ready.envelope.usage = success.usage;
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                    }

                    for (name, payload) in anthropic::stream_stop_events(&ready.envelope) {
                        let mut event = Event::default().data(payload);
                        if let Some(name) = name {
                            event = event.event(name);
                        }
                        yield Ok::<Event, Infallible>(event);
                    }
                };
                with_account_headers(
                    Sse::new(stream).into_response(),
                    &account_id,
                    &account_label,
                )
            }
            Err(error) => anthropic_error_response(error),
        };
    }

    match state.execute(&turn).await {
        Ok((envelope, account_id, account_label)) => with_account_headers(
            Json(anthropic::message_response(&envelope)).into_response(),
            &account_id,
            &account_label,
        ),
        Err(error) => anthropic_error_response(error),
    }
}

async fn openai_models(State(state): State<Arc<ServiceState>>) -> Response {
    Json(openai::models_response(&state.config)).into_response()
}

async fn healthz(State(state): State<Arc<ServiceState>>) -> Response {
    let account_count = state
        .scheduler
        .snapshots()
        .map(|items| items.len())
        .unwrap_or_default();
    Json(HealthResponse {
        ok: account_count > 0,
        account_count,
    })
    .into_response()
}

async fn admin_accounts(State(state): State<Arc<ServiceState>>) -> Response {
    let payload = state
        .scheduler
        .snapshots()
        .unwrap_or_default()
        .into_iter()
        .map(|snapshot| AdminAccountResponse {
            id: snapshot.summary.id,
            label: snapshot.summary.label,
            account_id: snapshot.summary.account_id,
            status: snapshot.summary.status.to_string(),
            inflight: snapshot.inflight,
            cooldown_until: snapshot.cooldown_until,
            current: snapshot.summary.is_current,
            five_hour_used_percent: snapshot
                .summary
                .usage
                .as_ref()
                .and_then(|usage| usage.five_hour.as_ref())
                .map(|window| window.used_percent),
            one_week_used_percent: snapshot
                .summary
                .usage
                .as_ref()
                .and_then(|usage| usage.one_week.as_ref())
                .map(|window| window.used_percent),
            usage_error: snapshot.summary.usage_error,
        })
        .collect::<Vec<_>>();
    Json(payload).into_response()
}

fn authorize_openai(headers: &HeaderMap, expected_api_key: &str) -> Result<(), ProtocolError> {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(ProtocolError::new(
            StatusCode::UNAUTHORIZED,
            "missing Authorization header",
        ));
    };
    let actual = value.to_str().map_err(|_| {
        ProtocolError::new(StatusCode::UNAUTHORIZED, "invalid Authorization header")
    })?;
    let expected = format!("Bearer {expected_api_key}");
    if actual != expected {
        return Err(ProtocolError::new(
            StatusCode::UNAUTHORIZED,
            "invalid API key",
        ));
    }
    Ok(())
}

fn authorize_anthropic(headers: &HeaderMap, expected_api_key: &str) -> Result<(), ProtocolError> {
    let version = headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ProtocolError::bad_request("missing anthropic-version header"))?;
    if version != anthropic::REQUIRED_VERSION {
        return Err(ProtocolError::bad_request(format!(
            "unsupported anthropic-version: {version}"
        )));
    }
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ProtocolError::new(StatusCode::UNAUTHORIZED, "missing x-api-key header"))?;
    if api_key != expected_api_key {
        return Err(ProtocolError::new(
            StatusCode::UNAUTHORIZED,
            "invalid API key",
        ));
    }
    Ok(())
}

fn with_account_headers(mut response: Response, account_id: &str, account_label: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(account_id) {
        response
            .headers_mut()
            .insert("X-Codex-Pool-Account-Id", value);
    }
    if let Ok(value) = HeaderValue::from_str(account_label) {
        response
            .headers_mut()
            .insert("X-Codex-Pool-Account-Label", value);
    }
    response
}

fn openai_error_response(error: ProtocolError) -> Response {
    let payload = serde_json::json!({
        "error": {
            "message": error.message,
            "type": "invalid_request_error",
        }
    });
    (error.status, Json(payload)).into_response()
}

fn anthropic_error_response(error: ProtocolError) -> Response {
    (error.status, Json(anthropic::error_response(error.message))).into_response()
}

pub fn resolve_model_alias(
    requested: &str,
    aliases: &BTreeMap<String, String>,
) -> Result<(String, String), ProtocolError> {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return Err(ProtocolError::bad_request("model must not be empty"));
    }
    let resolved = aliases
        .get(trimmed)
        .cloned()
        .ok_or_else(|| ProtocolError::bad_request(format!("unknown model alias: {trimmed}")))?;
    Ok((trimmed.to_string(), resolved))
}

use anyhow::Context;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use reqwest::Client;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use crate::auth;
    use crate::context::AppPaths;
    use crate::context::MockUsageResponse;
    use crate::models::AccountsStore;
    use crate::models::UsageSnapshot;
    use crate::models::UsageWindow;

    use super::*;

    fn build_auth(account_id: &str, email: &str) -> serde_json::Value {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("access-{account_id}"),
                "refresh_token": format!("refresh-{account_id}"),
                "account_id": account_id,
                "id_token": format!("header.{}.sig", base64_url_payload(email, account_id))
            }
        })
    }

    fn base64_url_payload(email: &str, account_id: &str) -> String {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        URL_SAFE_NO_PAD.encode(format!(
            "{{\"email\":\"{email}\",\"https://api.openai.com/auth\":{{\"chatgpt_account_id\":\"{account_id}\",\"chatgpt_plan_type\":\"pro\"}}}}"
        ))
    }

    fn usage_snapshot(one_week_used: f64, five_hour_used: f64) -> UsageSnapshot {
        UsageSnapshot {
            fetched_at: 1,
            plan_type: Some("pro".to_string()),
            five_hour: Some(UsageWindow {
                used_percent: five_hour_used,
                window_seconds: 18_000,
                reset_at: Some(10),
            }),
            one_week: Some(UsageWindow {
                used_percent: one_week_used,
                window_seconds: 604_800,
                reset_at: Some(20),
            }),
            credits: None,
        }
    }

    fn write_fake_codex_script(temp: &TempDir) -> PathBuf {
        let script_path = temp.path().join("codex");
        let script = r#"#!/bin/sh
if [ "$1" != "app-server" ]; then
  echo "unsupported fake codex command: $*" >&2
  exit 1
fi

auth="$HOME/.codex/auth.json"
if grep -q "fail-1" "$auth"; then
  echo "provided authentication token is expired" >&2
  exit 1
fi
account=$(grep -o '"account_id": "[^"]*"' "$auth" | head -n 1 | sed 's/.*"account_id": "\(.*\)"/\1/')
thread_id="thread-${account}"
turn_id="turn-${account}-1"

while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"protocolVersion\":\"1\",\"serverInfo\":{\"name\":\"fake-codex\",\"version\":\"0.0.0\"}}}"
      ;;
    thread/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"thread\":{\"id\":\"${thread_id}\"}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"thread/started\",\"params\":{\"threadId\":\"${thread_id}\"}}"
      ;;
    turn/start)
      if [ "$account" = "rate-1" ]; then
        printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"error\":{\"message\":\"429 usage_limit_exceeded\"}}"
        continue
      fi
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"turn\":{\"id\":\"${turn_id}\"}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{\"threadId\":\"${thread_id}\",\"turnId\":\"${turn_id}\"}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"item/started\",\"params\":{\"threadId\":\"${thread_id}\",\"turnId\":\"${turn_id}\",\"item\":{\"id\":\"item-${account}\",\"type\":\"agentMessage\",\"text\":\"\"}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"${thread_id}\",\"turnId\":\"${turn_id}\",\"itemId\":\"item-${account}\",\"delta\":\"response \"}}"
      sleep 1
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"${thread_id}\",\"turnId\":\"${turn_id}\",\"itemId\":\"item-${account}\",\"delta\":\"from ${account}\"}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"item/completed\",\"params\":{\"threadId\":\"${thread_id}\",\"turnId\":\"${turn_id}\",\"itemId\":\"item-${account}\",\"item\":{\"id\":\"item-${account}\",\"type\":\"agentMessage\",\"text\":\"response from ${account}\"}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"thread/tokenUsage/updated\",\"params\":{\"threadId\":\"${thread_id}\",\"tokenUsage\":{\"last\":{\"inputTokens\":12,\"outputTokens\":4}}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"threadId\":\"${thread_id}\",\"turnId\":\"${turn_id}\"}}"
      ;;
    thread/unsubscribe)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{}}"
      ;;
  esac
done
"#;
        fs::write(&script_path, script).expect("script should be written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script_path)
                .expect("metadata should exist")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script_path, perms).expect("permissions should update");
        }
        script_path
    }

    async fn spawn_test_server(temp: &TempDir) -> (String, oneshot::Sender<()>, AppContext) {
        let paths = AppPaths::for_home(temp.path().to_path_buf());
        let mut context = AppContext::with_paths(paths);
        context.ensure_layout().expect("layout should exist");
        context.codex_cli_path = Some(write_fake_codex_script(temp));
        context.test_hooks.mock_usage.insert(
            "rate-1".to_string(),
            MockUsageResponse::Snapshot(usage_snapshot(10.0, 10.0)),
        );
        context.test_hooks.mock_usage.insert(
            "good-1".to_string(),
            MockUsageResponse::Snapshot(usage_snapshot(20.0, 20.0)),
        );

        let original_live_auth = build_auth("live-1", "live@example.com");
        auth::write_active_codex_auth(&context, &original_live_auth)
            .expect("live auth should exist");

        let mut store_value = AccountsStore::default();
        let _ = crate::store::upsert_auth_account(
            &mut store_value,
            build_auth("rate-1", "rate@example.com"),
            Some("Rate".to_string()),
            Some(usage_snapshot(10.0, 10.0)),
            None,
        )
        .expect("account should upsert");
        let _ = crate::store::upsert_auth_account(
            &mut store_value,
            build_auth("good-1", "good@example.com"),
            Some("Good".to_string()),
            Some(usage_snapshot(20.0, 20.0)),
            None,
        )
        .expect("account should upsert");
        crate::store::save_store(&context, &store_value).expect("store should save");

        let (tx, rx) = oneshot::channel();
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
        let listen = probe
            .local_addr()
            .expect("local addr should exist")
            .to_string();
        drop(probe);
        let context_clone = context.clone();
        let listen_for_task = listen.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_with_shutdown(
                context_clone,
                ServeOptions {
                    listen: Some(listen_for_task),
                    api_key: Some("test-key".to_string()),
                    ..ServeOptions::default()
                },
                async move {
                    let _ = rx.await;
                },
            )
            .await
            {
                eprintln!("test proxy server exited early: {error:#}");
            }
        });
        let client = Client::new();
        let base_url = format!("http://{listen}");
        for _ in 0..20 {
            if let Ok(response) = client.get(format!("{base_url}/healthz")).send().await {
                if response.status().is_success() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        (base_url, tx, context)
    }

    #[tokio::test]
    async fn openai_and_anthropic_endpoints_work_and_preserve_live_auth() {
        let temp = TempDir::new().expect("tempdir should exist");
        let (base_url, shutdown, context) = spawn_test_server(&temp).await;
        let client = Client::new();

        let openai_response = client
            .post(format!("{base_url}/v1/chat/completions"))
            .header("Authorization", "Bearer test-key")
            .json(&serde_json::json!({
                "model": "codex",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("openai request should succeed");
        assert_eq!(openai_response.status(), StatusCode::OK);
        let openai_json: serde_json::Value =
            openai_response.json().await.expect("json should parse");
        assert_eq!(
            openai_json["choices"][0]["message"]["content"],
            "response from good-1"
        );

        let anthropic_response = client
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", "test-key")
            .header("anthropic-version", anthropic::REQUIRED_VERSION)
            .json(&serde_json::json!({
                "model": "codex",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("anthropic request should succeed");
        assert_eq!(anthropic_response.status(), StatusCode::OK);
        let anthropic_json: serde_json::Value =
            anthropic_response.json().await.expect("json should parse");
        assert_eq!(anthropic_json["content"][0]["text"], "response from good-1");

        let live = auth::read_current_codex_auth(&context).expect("live auth should remain");
        assert_eq!(
            auth::extract_auth(&live)
                .expect("live auth should parse")
                .account_id,
            "live-1"
        );

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn admin_endpoint_reports_inflight_and_cooldown() {
        let temp = TempDir::new().expect("tempdir should exist");
        let (base_url, shutdown, _context) = spawn_test_server(&temp).await;
        let client = Client::new();

        let response = client
            .get(format!("{base_url}/admin/accounts"))
            .send()
            .await
            .expect("admin request should succeed");
        assert_eq!(response.status(), StatusCode::OK);
        let payload: serde_json::Value = response.json().await.expect("json should parse");
        let accounts = payload.as_array().expect("array");
        assert!(accounts.len() >= 2);
        assert!(accounts[0].get("inflight").is_some());
        assert!(accounts[0].get("cooldown_until").is_some());
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn streaming_endpoints_return_protocol_native_sse_shapes() {
        let temp = TempDir::new().expect("tempdir should exist");
        let (base_url, shutdown, _context) = spawn_test_server(&temp).await;
        let client = Client::new();

        let openai_response = client
            .post(format!("{base_url}/v1/chat/completions"))
            .header("Authorization", "Bearer test-key")
            .json(&serde_json::json!({
                "model": "codex",
                "stream": true,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("openai stream should succeed");
        let mut openai_response = openai_response;
        let first_openai_chunk =
            tokio::time::timeout(Duration::from_millis(700), openai_response.chunk())
                .await
                .expect("openai first chunk should arrive before process completion")
                .expect("openai chunk should read")
                .expect("openai chunk should exist");
        let first_openai_text = String::from_utf8_lossy(&first_openai_chunk).to_string();
        assert!(first_openai_text.contains("chat.completion.chunk"));
        let mut openai_body = first_openai_text;
        while let Some(chunk) = openai_response.chunk().await.expect("chunk should read") {
            openai_body.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(openai_body.contains("chat.completion.chunk"));
        assert!(openai_body.contains("data: [DONE]"));

        let anthropic_response = client
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", "test-key")
            .header("anthropic-version", anthropic::REQUIRED_VERSION)
            .json(&serde_json::json!({
                "model": "codex",
                "stream": true,
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("anthropic stream should succeed");
        let mut anthropic_response = anthropic_response;
        let first_anthropic_chunk =
            tokio::time::timeout(Duration::from_millis(700), anthropic_response.chunk())
                .await
                .expect("anthropic first chunk should arrive before process completion")
                .expect("anthropic chunk should read")
                .expect("anthropic chunk should exist");
        let first_anthropic_text = String::from_utf8_lossy(&first_anthropic_chunk).to_string();
        assert!(first_anthropic_text.contains("event: message_start"));
        let mut anthropic_body = first_anthropic_text;
        while let Some(chunk) = anthropic_response.chunk().await.expect("chunk should read") {
            anthropic_body.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(anthropic_body.contains("event: message_start"));
        assert!(anthropic_body.contains("event: content_block_delta"));
        assert!(anthropic_body.contains("event: message_stop"));

        let _ = shutdown.send(());
    }
}
