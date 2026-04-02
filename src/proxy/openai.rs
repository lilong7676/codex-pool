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

#[derive(Debug, Deserialize)]
pub struct ChatCompletionsRequest {
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    pub tools: Option<Value>,
    pub tool_choice: Option<Value>,
    pub response_format: Option<Value>,
    pub n: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: ChatContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelDescriptor>,
}

#[derive(Debug, Serialize)]
pub struct ModelDescriptor {
    pub id: String,
    pub object: &'static str,
    pub owned_by: &'static str,
}

pub fn into_turn_request(
    request: ChatCompletionsRequest,
    config: &ResolvedProxyConfig,
) -> Result<ProxyTurnRequest, ProtocolError> {
    if request.tools.is_some() {
        return Err(ProtocolError::bad_request("OpenAI tools are not supported"));
    }
    if request.tool_choice.is_some() {
        return Err(ProtocolError::bad_request(
            "OpenAI tool_choice is not supported",
        ));
    }
    if request.response_format.is_some() {
        return Err(ProtocolError::bad_request(
            "OpenAI response_format is not supported",
        ));
    }
    if request.n.unwrap_or(1) != 1 {
        return Err(ProtocolError::bad_request("OpenAI n must be 1"));
    }
    if request.messages.is_empty() {
        return Err(ProtocolError::bad_request(
            "OpenAI messages must not be empty",
        ));
    }

    let (model_alias, resolved_model) = resolve_model_alias(
        request.model.as_deref().unwrap_or("codex"),
        &config.model_aliases,
    )?;

    let mut system_prompt: Option<String> = None;
    let mut messages = Vec::new();
    for message in request.messages {
        let role = match message.role.as_str() {
            "system" => ProxyRole::System,
            "user" => ProxyRole::User,
            "assistant" => ProxyRole::Assistant,
            other => {
                return Err(ProtocolError::new(
                    StatusCode::BAD_REQUEST,
                    format!("unsupported OpenAI role: {other}"),
                ));
            }
        };
        let content = flatten_content(message.content)?;
        if role == ProxyRole::System {
            match &mut system_prompt {
                Some(existing) => {
                    existing.push_str("\n\n");
                    existing.push_str(&content);
                }
                None => system_prompt = Some(content),
            }
        } else {
            messages.push(ProxyTurnMessage { role, content });
        }
    }
    if messages.is_empty() {
        return Err(ProtocolError::bad_request(
            "OpenAI messages must include at least one non-system message",
        ));
    }

    Ok(ProxyTurnRequest {
        protocol: super::ProtocolKind::OpenAi,
        model_alias,
        resolved_model,
        system_prompt,
        messages,
        stream: request.stream,
        cwd: config.default_cwd.clone(),
    })
}

fn flatten_content(content: ChatContent) -> Result<String, ProtocolError> {
    match content {
        ChatContent::Text(value) => {
            if value.trim().is_empty() {
                Err(ProtocolError::bad_request(
                    "OpenAI message content must not be empty",
                ))
            } else {
                Ok(value)
            }
        }
        ChatContent::Parts(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                if part.part_type != "text" {
                    return Err(ProtocolError::bad_request(
                        "OpenAI content parts only support text",
                    ));
                }
                let Some(text) = part.text else {
                    return Err(ProtocolError::bad_request(
                        "OpenAI text content part is missing text",
                    ));
                };
                texts.push(text);
            }
            let joined = texts.join("");
            if joined.trim().is_empty() {
                Err(ProtocolError::bad_request(
                    "OpenAI message content must not be empty",
                ))
            } else {
                Ok(joined)
            }
        }
    }
}

pub fn models_response(config: &ResolvedProxyConfig) -> ModelsResponse {
    ModelsResponse {
        object: "list",
        data: config
            .model_aliases
            .keys()
            .map(|alias| ModelDescriptor {
                id: alias.clone(),
                object: "model",
                owned_by: "codex-pool",
            })
            .collect(),
    }
}

pub fn completion_response(envelope: &ProxyExecutionEnvelope) -> Value {
    json!({
        "id": envelope.response_id,
        "object": "chat.completion",
        "created": envelope.created,
        "model": envelope.model_alias,
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": envelope.text,
                },
                "finish_reason": "stop"
            }
        ],
        "usage": {
            "prompt_tokens": envelope.usage.input_tokens,
            "completion_tokens": envelope.usage.output_tokens,
            "total_tokens": envelope.usage.input_tokens + envelope.usage.output_tokens,
        }
    })
}

pub fn stream_role_chunk(envelope: &ProxyExecutionEnvelope) -> String {
    json!({
        "id": envelope.response_id,
        "object": "chat.completion.chunk",
        "created": envelope.created,
        "model": envelope.model_alias,
        "choices": [
            {
                "index": 0,
                "delta": {
                    "role": "assistant"
                },
                "finish_reason": Value::Null,
            }
        ]
    })
    .to_string()
}

pub fn stream_text_chunk(envelope: &ProxyExecutionEnvelope, text: &str) -> String {
    json!({
        "id": envelope.response_id,
        "object": "chat.completion.chunk",
        "created": envelope.created,
        "model": envelope.model_alias,
        "choices": [
            {
                "index": 0,
                "delta": {
                    "content": text
                },
                "finish_reason": Value::Null,
            }
        ]
    })
    .to_string()
}

pub fn stream_stop_chunk(envelope: &ProxyExecutionEnvelope) -> String {
    json!({
        "id": envelope.response_id,
        "object": "chat.completion.chunk",
        "created": envelope.created,
        "model": envelope.model_alias,
        "choices": [
            {
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
            }
        ]
    })
    .to_string()
}

pub fn build_envelope(
    turn: &ProxyTurnRequest,
    result: &ProxyExecutionEnvelope,
) -> ProxyExecutionEnvelope {
    ProxyExecutionEnvelope {
        response_id: result.response_id.clone(),
        created: result.created,
        model_alias: turn.model_alias.clone(),
        text: result.text.clone(),
        usage: result.usage.clone(),
    }
}

pub fn new_execution_envelope(
    model_alias: &str,
    text: &str,
    usage: crate::proxy::runtime::RuntimeUsage,
) -> ProxyExecutionEnvelope {
    ProxyExecutionEnvelope {
        response_id: format!("chatcmpl_{}", Uuid::new_v4().simple()),
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
    fn rejects_unsupported_openai_fields() {
        let request = ChatCompletionsRequest {
            model: None,
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatContent::Text("hi".to_string()),
            }],
            stream: false,
            tools: Some(json!([])),
            tool_choice: None,
            response_format: None,
            n: None,
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
