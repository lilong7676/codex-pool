use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStderr;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;

use crate::context::AppContext;
use crate::models::StoredAccount;
use crate::utils::truncate_for_error;

use super::ProxyTurnMessage;
use super::ProxyTurnRequest;
use super::ResolvedProxyConfig;
use super::runtime::RuntimeUsage;

#[derive(Debug, Clone)]
pub struct BackendError {
    pub message: String,
    pub kind: BackendErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    Retriable,
    Fatal,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    TextDelta(String),
    Completed(BackendSuccess),
}

#[derive(Debug, Clone)]
pub struct BackendSuccess {
    pub text: String,
    pub usage: RuntimeUsage,
}

#[derive(Default)]
struct WorkerSlot {
    process: Option<BackendProcess>,
}

struct BackendProcess {
    _child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    stderr: tokio::io::Lines<BufReader<ChildStderr>>,
    next_request_id: u64,
    pending_notifications: VecDeque<Value>,
    _home_dir: tempfile::TempDir,
}

struct AccountBackend {
    slots: Vec<Arc<Mutex<WorkerSlot>>>,
    next_slot: AtomicUsize,
    context: AppContext,
    config: ResolvedProxyConfig,
}

pub struct BackendManager {
    context: AppContext,
    config: ResolvedProxyConfig,
    workers: Mutex<HashMap<String, Arc<AccountBackend>>>,
}

pub struct BackendExecution {
    worker: OwnedMutexGuard<WorkerSlot>,
    thread_id: String,
    turn_id: String,
    text: String,
    usage: RuntimeUsage,
    finished: bool,
    recent_stderr: Vec<String>,
}

impl BackendManager {
    pub fn new(context: AppContext, config: ResolvedProxyConfig) -> Arc<Self> {
        Arc::new(Self {
            context,
            config,
            workers: Mutex::new(HashMap::new()),
        })
    }

    pub async fn start_turn(
        &self,
        account: &StoredAccount,
        request: &ProxyTurnRequest,
    ) -> Result<BackendExecution, BackendError> {
        let backend = self.get_backend(&account.account_id).await;
        backend.start_turn(account, request).await
    }

    pub async fn invalidate(&self, account_id: &str) {
        if let Some(worker) = self.workers.lock().await.get(account_id).cloned() {
            worker.invalidate().await;
        }
    }

    async fn get_backend(&self, account_id: &str) -> Arc<AccountBackend> {
        let mut workers = self.workers.lock().await;
        if let Some(existing) = workers.get(account_id) {
            return Arc::clone(existing);
        }
        let worker_count = self.config.max_inflight_per_account.max(1);
        let backend = Arc::new(AccountBackend {
            slots: (0..worker_count)
                .map(|_| Arc::new(Mutex::new(WorkerSlot::default())))
                .collect(),
            next_slot: AtomicUsize::new(0),
            context: self.context.clone(),
            config: self.config.clone(),
        });
        workers.insert(account_id.to_string(), Arc::clone(&backend));
        backend
    }
}

impl AccountBackend {
    async fn start_turn(
        &self,
        account: &StoredAccount,
        request: &ProxyTurnRequest,
    ) -> Result<BackendExecution, BackendError> {
        let slot_index = self.next_slot.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut slot = Arc::clone(&self.slots[slot_index]).lock_owned().await;
        ensure_process(&self.context, &self.config, account, &mut slot).await?;
        let process = slot.process.as_mut().ok_or_else(|| BackendError {
            message: "backend worker failed to initialize".to_string(),
            kind: BackendErrorKind::Fatal,
        })?;

        let thread_response = send_request(
            process,
            "thread/start",
            serde_json::json!({
                "approvalPolicy": self.config.approval_policy,
                "sandbox": self.config.sandbox,
                "cwd": request.cwd.display().to_string(),
                "model": request.resolved_model,
                "ephemeral": true,
                "personality": "pragmatic"
            }),
        )
        .await?;
        let thread_id = thread_response
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| BackendError {
                message: "thread/start did not return thread id".to_string(),
                kind: BackendErrorKind::Fatal,
            })?;

        let turn_response = send_request(
            process,
            "turn/start",
            serde_json::json!({
                "threadId": thread_id,
                "input": [
                    {
                        "type": "text",
                        "text": build_prompt(request),
                    }
                ]
            }),
        )
        .await?;
        let turn_id = turn_response
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| BackendError {
                message: "turn/start did not return turn id".to_string(),
                kind: BackendErrorKind::Fatal,
            })?;

        Ok(BackendExecution {
            worker: slot,
            thread_id,
            turn_id,
            text: String::new(),
            usage: RuntimeUsage::default(),
            finished: false,
            recent_stderr: Vec::new(),
        })
    }

    async fn invalidate(&self) {
        for slot in &self.slots {
            let mut slot = slot.lock().await;
            slot.process = None;
        }
    }
}

impl BackendExecution {
    pub async fn next(&mut self) -> Option<Result<BackendEvent, BackendError>> {
        if self.finished {
            return None;
        }

        loop {
            if let Some(value) = self
                .worker
                .process
                .as_mut()
                .and_then(|process| process.pending_notifications.pop_front())
            {
                if let Some(result) = self.handle_notification(value).await {
                    return Some(result);
                }
                continue;
            }

            let next = {
                let process = match self.worker.process.as_mut() {
                    Some(process) => process,
                    None => {
                        self.finished = true;
                        return Some(Err(BackendError {
                            message: "backend worker became unavailable".to_string(),
                            kind: BackendErrorKind::Fatal,
                        }));
                    }
                };
                read_backend_line(process).await
            };
            let line = match next {
                Ok(Some(line)) => line,
                Ok(None) => {
                    self.worker.process = None;
                    self.finished = true;
                    return Some(Err(classify_backend_error(
                        &self.text,
                        &self.recent_stderr.join("\n"),
                    )));
                }
                Err(error) => {
                    self.worker.process = None;
                    self.finished = true;
                    return Some(Err(error));
                }
            };

            match line {
                BackendLine::Stderr(stderr) => {
                    if !stderr.trim().is_empty() {
                        self.recent_stderr.push(stderr);
                    }
                }
                BackendLine::Stdout(stdout) => {
                    let Ok(value) = serde_json::from_str::<Value>(&stdout) else {
                        continue;
                    };
                    if let Some(result) = self.handle_notification(value).await {
                        return Some(result);
                    }
                }
            }
        }
    }

    async fn handle_notification(
        &mut self,
        value: Value,
    ) -> Option<Result<BackendEvent, BackendError>> {
        let method = value.get("method").and_then(Value::as_str)?;
        let params = value.get("params").unwrap_or(&Value::Null);
        if params
            .get("threadId")
            .and_then(Value::as_str)
            .map(|thread| thread != self.thread_id)
            .unwrap_or(false)
        {
            return None;
        }
        if params
            .get("turnId")
            .and_then(Value::as_str)
            .map(|turn| turn != self.turn_id)
            .unwrap_or(false)
        {
            return None;
        }

        match method {
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if delta.is_empty() {
                    return None;
                }
                self.text.push_str(&delta);
                Some(Ok(BackendEvent::TextDelta(delta)))
            }
            "item/completed" => {
                let item = params.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
                    return None;
                }
                let Some(full_text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                else {
                    return None;
                };
                if full_text.starts_with(&self.text) {
                    let delta = full_text[self.text.len()..].to_string();
                    if !delta.is_empty() {
                        self.text = full_text;
                        return Some(Ok(BackendEvent::TextDelta(delta)));
                    }
                } else if self.text.is_empty() {
                    self.text = full_text.clone();
                    return Some(Ok(BackendEvent::TextDelta(full_text)));
                }
                None
            }
            "thread/tokenUsage/updated" => {
                if let Some(last) = params.get("tokenUsage").and_then(|usage| usage.get("last")) {
                    self.usage.input_tokens = last
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.usage.input_tokens);
                    self.usage.output_tokens = last
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.usage.output_tokens);
                }
                None
            }
            "turn/completed" => {
                if let Some(process) = self.worker.process.as_mut() {
                    let _ = cleanup_thread(process, &self.thread_id).await;
                }
                self.finished = true;
                Some(Ok(BackendEvent::Completed(BackendSuccess {
                    text: self.text.clone(),
                    usage: self.usage.clone(),
                })))
            }
            "error" => {
                self.worker.process = None;
                self.finished = true;
                Some(Err(classify_backend_error(
                    &self.text,
                    &format!("{}\n{}", self.recent_stderr.join("\n"), value),
                )))
            }
            _ => None,
        }
    }
}

async fn ensure_process(
    context: &AppContext,
    config: &ResolvedProxyConfig,
    account: &StoredAccount,
    slot: &mut WorkerSlot,
) -> Result<(), BackendError> {
    let should_spawn = match slot.process.as_mut() {
        Some(process) => process._child.try_wait().ok().flatten().is_some(),
        None => true,
    };
    if !should_spawn {
        return Ok(());
    }

    slot.process = Some(spawn_process(context, config, account).await?);
    Ok(())
}

async fn spawn_process(
    context: &AppContext,
    config: &ResolvedProxyConfig,
    account: &StoredAccount,
) -> Result<BackendProcess, BackendError> {
    let temp_dir = tempfile::tempdir().map_err(|error| BackendError {
        message: format!("failed to create backend home: {error}"),
        kind: BackendErrorKind::Fatal,
    })?;
    prepare_home(context, temp_dir.path(), account).map_err(|error| BackendError {
        message: error.to_string(),
        kind: BackendErrorKind::Fatal,
    })?;

    let codex_path = context.codex_cli_path.clone().ok_or_else(|| BackendError {
        message: "codex executable not found in PATH".to_string(),
        kind: BackendErrorKind::Fatal,
    })?;
    let mut command = Command::new(&codex_path);
    if let Some(parent) = codex_path.parent() {
        let merged_path = if let Some(current_path) = std::env::var_os("PATH") {
            let path_entries = std::iter::once(parent.to_path_buf())
                .chain(std::env::split_paths(&current_path))
                .collect::<Vec<_>>();
            std::env::join_paths(path_entries).context("failed to build PATH")
        } else {
            std::env::join_paths([parent]).context("failed to build PATH")
        }
        .map_err(|error| BackendError {
            message: error.to_string(),
            kind: BackendErrorKind::Fatal,
        })?;
        command.env("PATH", merged_path);
    }

    command
        .env("HOME", temp_dir.path())
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| BackendError {
        message: format!("failed to spawn codex app-server: {error}"),
        kind: BackendErrorKind::Fatal,
    })?;
    let stdin = child.stdin.take().ok_or_else(|| BackendError {
        message: "failed to capture backend stdin".to_string(),
        kind: BackendErrorKind::Fatal,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| BackendError {
        message: "failed to capture backend stdout".to_string(),
        kind: BackendErrorKind::Fatal,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| BackendError {
        message: "failed to capture backend stderr".to_string(),
        kind: BackendErrorKind::Fatal,
    })?;

    let mut process = BackendProcess {
        _child: child,
        stdin,
        stdout: BufReader::new(stdout).lines(),
        stderr: BufReader::new(stderr).lines(),
        next_request_id: 1,
        pending_notifications: VecDeque::new(),
        _home_dir: temp_dir,
    };

    let _ = send_request(
        &mut process,
        "initialize",
        serde_json::json!({
            "clientInfo": {
                "name": "codex-pool",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
    .await?;

    let _ = config;
    Ok(process)
}

async fn send_request(
    process: &mut BackendProcess,
    method: &str,
    params: Value,
) -> Result<Value, BackendError> {
    let id = process.next_request_id;
    process.next_request_id += 1;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let serialized = serde_json::to_string(&request).map_err(|error| BackendError {
        message: format!("failed to serialize backend request: {error}"),
        kind: BackendErrorKind::Fatal,
    })?;
    process
        .stdin
        .write_all(serialized.as_bytes())
        .await
        .map_err(|error| BackendError {
            message: format!("failed to write backend request: {error}"),
            kind: BackendErrorKind::Fatal,
        })?;
    process
        .stdin
        .write_all(b"\n")
        .await
        .map_err(|error| BackendError {
            message: format!("failed to write backend request newline: {error}"),
            kind: BackendErrorKind::Fatal,
        })?;
    process.stdin.flush().await.map_err(|error| BackendError {
        message: format!("failed to flush backend request: {error}"),
        kind: BackendErrorKind::Fatal,
    })?;

    let mut stderr_lines = Vec::new();
    loop {
        match read_backend_line(process).await? {
            Some(BackendLine::Stderr(stderr)) => stderr_lines.push(stderr),
            Some(BackendLine::Stdout(stdout)) => {
                let Ok(value) = serde_json::from_str::<Value>(&stdout) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(result) = value.get("result") {
                        return Ok(result.clone());
                    }
                    let message = value
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("backend request failed")
                        .to_string();
                    return Err(classify_backend_error(
                        "",
                        &format!("{}\n{}", stderr_lines.join("\n"), message),
                    ));
                }
                if value.get("method").and_then(Value::as_str).is_some() {
                    process.pending_notifications.push_back(value);
                }
            }
            None => {
                return Err(classify_backend_error("", &stderr_lines.join("\n")));
            }
        }
    }
}

async fn cleanup_thread(process: &mut BackendProcess, thread_id: &str) -> Result<(), BackendError> {
    let _ = send_request(
        process,
        "thread/unsubscribe",
        serde_json::json!({ "threadId": thread_id }),
    )
    .await;
    Ok(())
}

enum BackendLine {
    Stdout(String),
    Stderr(String),
}

async fn read_backend_line(
    process: &mut BackendProcess,
) -> Result<Option<BackendLine>, BackendError> {
    tokio::select! {
        stdout = process.stdout.next_line() => {
            match stdout {
                Ok(Some(line)) => Ok(Some(BackendLine::Stdout(line))),
                Ok(None) => Ok(None),
                Err(error) => Err(BackendError {
                    message: format!("failed to read backend stdout: {error}"),
                    kind: BackendErrorKind::Fatal,
                }),
            }
        }
        stderr = process.stderr.next_line() => {
            match stderr {
                Ok(Some(line)) => Ok(Some(BackendLine::Stderr(line))),
                Ok(None) => Ok(None),
                Err(error) => Err(BackendError {
                    message: format!("failed to read backend stderr: {error}"),
                    kind: BackendErrorKind::Fatal,
                }),
            }
        }
    }
}

fn prepare_home(context: &AppContext, home: &Path, account: &StoredAccount) -> anyhow::Result<()> {
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir)
        .with_context(|| format!("failed to create {}", codex_dir.display()))?;
    let auth_path = codex_dir.join("auth.json");
    fs::write(
        &auth_path,
        serde_json::to_string_pretty(&account.auth_json)?,
    )
    .with_context(|| format!("failed to write {}", auth_path.display()))?;
    copy_if_exists(
        &context.paths.codex_config_path,
        &codex_dir.join("config.toml"),
    )?;
    copy_if_exists(
        &context
            .paths
            .home_dir
            .join(".codex")
            .join(".credentials.json"),
        &codex_dir.join(".credentials.json"),
    )?;
    Ok(())
}

fn copy_if_exists(source: &Path, target: &Path) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn build_prompt(request: &ProxyTurnRequest) -> String {
    let mut lines = vec![
        "You are serving a local API compatibility request through Codex CLI.".to_string(),
        "Answer the conversation directly as the assistant.".to_string(),
        "Return only the assistant response text.".to_string(),
    ];
    if let Some(system_prompt) = request.system_prompt.as_deref() {
        lines.push(String::new());
        lines.push("System prompt:".to_string());
        lines.push(system_prompt.to_string());
    }
    lines.push(String::new());
    lines.push("Conversation:".to_string());
    for message in &request.messages {
        lines.push(format_message(message));
    }
    lines.join("\n")
}

fn format_message(message: &ProxyTurnMessage) -> String {
    let role = match message.role {
        super::ProxyRole::System => "system",
        super::ProxyRole::User => "user",
        super::ProxyRole::Assistant => "assistant",
    };
    format!("[{role}]\n{}", message.content)
}

fn classify_backend_error(stdout: &str, stderr: &str) -> BackendError {
    let combined = format!("{stdout}\n{stderr}");
    let normalized = combined.to_ascii_lowercase();
    let retriable = normalized.contains("401")
        || normalized.contains("429")
        || normalized.contains("unauthorized")
        || normalized.contains("rate limit")
        || normalized.contains("usage_limit_exceeded")
        || normalized.contains("authorization expired")
        || normalized.contains("invalid refresh token")
        || normalized.contains("invalid_grant")
        || normalized.contains("token is expired")
        || normalized.contains("provided authentication token is expired")
        || normalized.contains("reauth_required")
        || normalized.contains("run codex login first");
    BackendError {
        message: truncate_for_error(combined.trim(), 400),
        kind: if retriable {
            BackendErrorKind::Retriable
        } else {
            BackendErrorKind::Fatal
        },
    }
}
