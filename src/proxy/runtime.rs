use std::fs;
use std::path::Path;
use std::process::Stdio;

use anyhow::Context;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::context::AppContext;
use crate::models::StoredAccount;
use crate::utils::truncate_for_error;

use super::ProxyTurnMessage;
use super::ProxyTurnRequest;
use super::ResolvedProxyConfig;

#[derive(Debug, Clone, Default)]
pub struct RuntimeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct RuntimeSuccess {
    pub text: String,
    pub usage: RuntimeUsage,
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub kind: RuntimeErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeErrorKind {
    Retriable,
    Fatal,
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    TextDelta(String),
    Completed(RuntimeSuccess),
}

pub struct RuntimeExecution {
    receiver: mpsc::Receiver<Result<RuntimeEvent, RuntimeError>>,
}

#[derive(Debug)]
enum RuntimeLine {
    Stdout(String),
    Stderr(String),
}

#[derive(Default)]
struct ParseState {
    text: String,
    usage: RuntimeUsage,
    stderr_lines: Vec<String>,
}

pub async fn execute(
    context: &AppContext,
    account: &StoredAccount,
    request: &ProxyTurnRequest,
    config: &ResolvedProxyConfig,
) -> Result<RuntimeSuccess, RuntimeError> {
    let mut execution = spawn(context, account, request, config).await?;
    let mut final_success = None;

    while let Some(event) = execution.next().await {
        match event? {
            RuntimeEvent::TextDelta(_) => {}
            RuntimeEvent::Completed(success) => final_success = Some(success),
        }
    }

    final_success.ok_or_else(|| RuntimeError {
        message: "codex exec finished without completion event".to_string(),
        kind: RuntimeErrorKind::Fatal,
    })
}

pub async fn spawn(
    context: &AppContext,
    account: &StoredAccount,
    request: &ProxyTurnRequest,
    config: &ResolvedProxyConfig,
) -> Result<RuntimeExecution, RuntimeError> {
    let temp_dir = tempfile::tempdir().map_err(|error| RuntimeError {
        message: format!("failed to create temporary proxy home: {error}"),
        kind: RuntimeErrorKind::Fatal,
    })?;
    prepare_home(context, temp_dir.path(), account).map_err(|error| RuntimeError {
        message: error.to_string(),
        kind: RuntimeErrorKind::Fatal,
    })?;

    let codex_path = context.codex_cli_path.clone().ok_or_else(|| RuntimeError {
        message: "codex executable not found in PATH".to_string(),
        kind: RuntimeErrorKind::Fatal,
    })?;
    let prompt = build_prompt(request);

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
        .map_err(|error| RuntimeError {
            message: error.to_string(),
            kind: RuntimeErrorKind::Fatal,
        })?;
        command.env("PATH", merged_path);
    }

    command
        .env("HOME", temp_dir.path())
        .arg("-a")
        .arg(&config.approval_policy)
        .arg("-s")
        .arg(&config.sandbox)
        .arg("exec")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&request.cwd)
        .arg("--model")
        .arg(&request.resolved_model)
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| RuntimeError {
        message: format!("failed to run codex exec: {error}"),
        kind: RuntimeErrorKind::Fatal,
    })?;

    let stdout = child.stdout.take().ok_or_else(|| RuntimeError {
        message: "failed to capture codex stdout".to_string(),
        kind: RuntimeErrorKind::Fatal,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| RuntimeError {
        message: "failed to capture codex stderr".to_string(),
        kind: RuntimeErrorKind::Fatal,
    })?;

    let (line_tx, mut line_rx) = mpsc::channel::<RuntimeLine>(128);
    let (event_tx, event_rx) = mpsc::channel::<Result<RuntimeEvent, RuntimeError>>(128);

    tokio::spawn({
        let line_tx = line_tx.clone();
        async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line_tx.send(RuntimeLine::Stdout(line)).await.is_err() {
                    break;
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(RuntimeLine::Stderr(line)).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let _temp_dir = temp_dir;
        let mut state = ParseState::default();
        while let Some(line) = line_rx.recv().await {
            match line {
                RuntimeLine::Stdout(stdout_line) => {
                    if let Some(result) = process_stdout_line(&stdout_line, &mut state) {
                        if event_tx.send(result).await.is_err() {
                            return;
                        }
                    }
                }
                RuntimeLine::Stderr(stderr_line) => {
                    state.stderr_lines.push(stderr_line);
                }
            }
        }

        let status = match child.wait().await {
            Ok(status) => status,
            Err(error) => {
                let _ = event_tx
                    .send(Err(RuntimeError {
                        message: format!("failed waiting for codex exec: {error}"),
                        kind: RuntimeErrorKind::Fatal,
                    }))
                    .await;
                return;
            }
        };

        if !status.success() {
            let _ = event_tx
                .send(Err(classify_runtime_error(
                    &state.text,
                    &state.stderr_lines.join("\n"),
                )))
                .await;
            return;
        }

        if state.text.trim().is_empty() {
            let _ = event_tx
                .send(Err(classify_runtime_error(
                    "",
                    &format!(
                        "{}\ncodex exec returned no assistant text",
                        state.stderr_lines.join("\n")
                    ),
                )))
                .await;
            return;
        }

        let _ = event_tx
            .send(Ok(RuntimeEvent::Completed(RuntimeSuccess {
                text: state.text,
                usage: state.usage,
            })))
            .await;
    });

    Ok(RuntimeExecution { receiver: event_rx })
}

impl RuntimeExecution {
    pub async fn next(&mut self) -> Option<Result<RuntimeEvent, RuntimeError>> {
        self.receiver.recv().await
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

fn process_stdout_line(
    line: &str,
    state: &mut ParseState,
) -> Option<Result<RuntimeEvent, RuntimeError>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return None;
    }

    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(_) => return None,
    };

    if let Some(text) = extract_text_delta(&value) {
        if !text.is_empty() {
            state.text.push_str(&text);
            return Some(Ok(RuntimeEvent::TextDelta(text)));
        }
    }

    if let Some(snapshot) = extract_completed_text(&value) {
        if snapshot.starts_with(&state.text) {
            let delta = snapshot[state.text.len()..].to_string();
            state.text = snapshot;
            if !delta.is_empty() {
                return Some(Ok(RuntimeEvent::TextDelta(delta)));
            }
        } else if state.text.is_empty() {
            state.text = snapshot.clone();
            return Some(Ok(RuntimeEvent::TextDelta(snapshot)));
        }
    }

    if value.get("type").and_then(Value::as_str) == Some("turn.completed") {
        if let Some(turn_usage) = value.get("usage") {
            state.usage.input_tokens = turn_usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            state.usage.output_tokens = turn_usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
        }
    }

    None
}

fn extract_text_delta(value: &Value) -> Option<String> {
    let root_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !root_type.contains("delta") {
        return None;
    }

    value
        .get("delta")
        .and_then(extract_textish)
        .or_else(|| {
            value
                .get("item")
                .and_then(|item| item.get("delta"))
                .and_then(extract_textish)
        })
        .or_else(|| {
            value.get("item").and_then(|item| {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
                if item_type.contains("delta") {
                    extract_textish(item)
                } else {
                    None
                }
            })
        })
        .or_else(|| extract_textish(value))
}

fn extract_completed_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("item.completed") {
        return None;
    }
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("agent_message") {
        return None;
    }
    item.get("text")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn extract_textish(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            value.get("content").and_then(|content| {
                if let Some(text) = content.as_str() {
                    return Some(text.to_string());
                }
                content
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
        })
}

fn classify_runtime_error(stdout: &str, stderr: &str) -> RuntimeError {
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
    RuntimeError {
        message: truncate_for_error(combined.trim(), 400),
        kind: if retriable {
            RuntimeErrorKind::Retriable
        } else {
            RuntimeErrorKind::Fatal
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::proxy::ProxyRole;

    #[test]
    fn prompt_builder_includes_messages_and_system() {
        let request = ProxyTurnRequest {
            protocol: super::super::ProtocolKind::OpenAi,
            model_alias: "codex".to_string(),
            resolved_model: "gpt-5.4".to_string(),
            system_prompt: Some("be concise".to_string()),
            messages: vec![
                ProxyTurnMessage {
                    role: ProxyRole::User,
                    content: "hello".to_string(),
                },
                ProxyTurnMessage {
                    role: ProxyRole::Assistant,
                    content: "hi".to_string(),
                },
            ],
            stream: false,
            cwd: PathBuf::from("/tmp"),
        };
        let prompt = build_prompt(&request);
        assert!(prompt.contains("System prompt:"));
        assert!(prompt.contains("[user]"));
        assert!(prompt.contains("[assistant]"));
    }

    #[test]
    fn delta_parser_accumulates_incrementally() {
        let mut state = ParseState::default();
        let first = process_stdout_line(
            r#"{"type":"item.delta","delta":{"text":"hello "}}"#,
            &mut state,
        )
        .expect("delta should be emitted")
        .expect("delta should succeed");
        let second = process_stdout_line(
            r#"{"type":"item.delta","delta":{"text":"world"}}"#,
            &mut state,
        )
        .expect("delta should be emitted")
        .expect("delta should succeed");

        match first {
            RuntimeEvent::TextDelta(value) => assert_eq!(value, "hello "),
            _ => panic!("unexpected event"),
        }
        match second {
            RuntimeEvent::TextDelta(value) => assert_eq!(value, "world"),
            _ => panic!("unexpected event"),
        }
        assert_eq!(state.text, "hello world");
    }
}
