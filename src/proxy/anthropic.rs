use axum::http::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::utils::now_unix_seconds;

use super::ProtocolError;
use super::ProxyExecutionEnvelope;
use super::ProxyRole;
use super::ProxyTurnMessage;
use super::ProxyTurnRequest;
use super::ResolvedProxyConfig;
use super::resolve_model_alias;

pub const REQUIRED_VERSION: &str = "2023-06-01";

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    pub tools: Option<Value>,
    pub thinking: Option<Value>,
    pub container: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: &'static str,
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    #[serde(rename = "type")]
    pub error_type: &'static str,
    pub message: String,
}

pub fn into_turn_request(
    request: MessagesRequest,
    config: &ResolvedProxyConfig,
) -> Result<ProxyTurnRequest, ProtocolError> {
    if request.tools.is_some() {
        return Err(ProtocolError::bad_request(
            "Anthropic tools are not supported",
        ));
    }
    if request.thinking.is_some() {
        return Err(ProtocolError::bad_request(
            "Anthropic thinking is not supported",
        ));
    }
    if request.container.is_some() {
        return Err(ProtocolError::bad_request(
            "Anthropic container is not supported",
        ));
    }
    if request.messages.is_empty() {
        return Err(ProtocolError::bad_request(
            "Anthropic messages must not be empty",
        ));
    }
    if let Some(max_tokens) = request.max_tokens {
        if max_tokens == 0 {
            return Err(ProtocolError::bad_request(
                "Anthropic max_tokens must be greater than zero",
            ));
        }
    }

    let (model_alias, resolved_model) = resolve_model_alias(
        request.model.as_deref().unwrap_or("codex"),
        &config.model_aliases,
    )?;

    let mut messages = Vec::new();
    for message in request.messages {
        let role = match message.role.as_str() {
            "user" => ProxyRole::User,
            "assistant" => ProxyRole::Assistant,
            other => {
                return Err(ProtocolError::new(
                    StatusCode::BAD_REQUEST,
                    format!("unsupported Anthropic role: {other}"),
                ));
            }
        };
        messages.push(ProxyTurnMessage {
            role,
            content: flatten_content(message.content)?,
        });
    }

    Ok(ProxyTurnRequest {
        protocol: super::ProtocolKind::Anthropic,
        model_alias,
        resolved_model,
        system_prompt: request.system,
        messages,
        stream: request.stream,
        cwd: config.default_cwd.clone(),
    })
}

fn flatten_content(content: MessageContent) -> Result<String, ProtocolError> {
    match content {
        MessageContent::Text(value) => {
            if value.trim().is_empty() {
                Err(ProtocolError::bad_request(
                    "Anthropic message content must not be empty",
                ))
            } else {
                Ok(value)
            }
        }
        MessageContent::Blocks(blocks) => {
            let mut texts = Vec::new();
            for block in blocks {
                if block.block_type != "text" {
                    return Err(ProtocolError::bad_request(
                        "Anthropic content blocks only support text",
                    ));
                }
                let Some(text) = block.text else {
                    return Err(ProtocolError::bad_request(
                        "Anthropic text block is missing text",
                    ));
                };
                texts.push(text);
            }
            let joined = texts.join("");
            if joined.trim().is_empty() {
                Err(ProtocolError::bad_request(
                    "Anthropic message content must not be empty",
                ))
            } else {
                Ok(joined)
            }
        }
    }
}

pub fn message_response(envelope: &ProxyExecutionEnvelope) -> Value {
    json!({
        "id": envelope.response_id,
        "type": "message",
        "role": "assistant",
        "model": envelope.model_alias,
        "content": [
            {
                "type": "text",
                "text": envelope.text,
            }
        ],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": envelope.usage.input_tokens,
            "output_tokens": envelope.usage.output_tokens,
        }
    })
}

pub fn stream_start_events(
    envelope: &ProxyExecutionEnvelope,
) -> Vec<(Option<&'static str>, String)> {
    vec![
        (
            Some("message_start"),
            json!({
                "type": "message_start",
                "message": {
                    "id": envelope.response_id,
                    "type": "message",
                    "role": "assistant",
                    "model": envelope.model_alias,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {
                        "input_tokens": envelope.usage.input_tokens,
                        "output_tokens": 0,
                    }
                }
            })
            .to_string(),
        ),
        (
            Some("content_block_start"),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "text",
                    "text": "",
                }
            })
            .to_string(),
        ),
    ]
}

pub fn stream_text_event(text: &str) -> (Option<&'static str>, String) {
    (
        Some("content_block_delta"),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": text,
            }
        })
        .to_string(),
    )
}

pub fn stream_stop_events(
    envelope: &ProxyExecutionEnvelope,
) -> Vec<(Option<&'static str>, String)> {
    vec![
        (
            Some("content_block_stop"),
            json!({
                "type": "content_block_stop",
                "index": 0,
            })
            .to_string(),
        ),
        (
            Some("message_delta"),
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "end_turn",
                    "stop_sequence": Value::Null,
                },
                "usage": {
                    "output_tokens": envelope.usage.output_tokens,
                }
            })
            .to_string(),
        ),
        (
            Some("message_stop"),
            json!({
                "type": "message_stop"
            })
            .to_string(),
        ),
    ]
}

pub fn error_response(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope {
        envelope_type: "error",
        error: ErrorBody {
            error_type: "invalid_request_error",
            message: message.into(),
        },
    }
}

pub fn new_execution_envelope(
    model_alias: &str,
    text: &str,
    usage: crate::proxy::runtime::RuntimeUsage,
) -> ProxyExecutionEnvelope {
    ProxyExecutionEnvelope {
        response_id: format!("msg_{}", Uuid::new_v4().simple()),
        created: now_unix_seconds(),
        model_alias: model_alias.to_string(),
        text: text.to_string(),
        usage,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn rejects_empty_max_tokens() {
        let request = MessagesRequest {
            model: None,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("hello".to_string()),
            }],
            max_tokens: Some(0),
            stream: false,
            tools: None,
            thinking: None,
            container: None,
        };
        let config = ResolvedProxyConfig {
            listen: "127.0.0.1:4141".to_string(),
            api_key: "secret".to_string(),
            default_cwd: PathBuf::from("/tmp"),
            default_model: "gpt-5.4".to_string(),
            sandbox: "workspace-write".to_string(),
            approval_policy: "never".to_string(),
            usage_refresh_interval_seconds: 60,
            max_concurrent_requests: 8,
            max_inflight_per_account: 1,
            model_aliases: BTreeMap::from([(String::from("codex"), String::from("gpt-5.4"))]),
        };
        let error = into_turn_request(request, &config).expect_err("request should fail");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }
}
