use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Map;
use serde_json::Value;

use crate::context::AppContext;
use crate::models::CurrentAuthStatus;
use crate::models::ExtractedAuth;
use crate::utils::set_private_permissions;
use crate::utils::truncate_for_error;

pub fn read_current_codex_auth(context: &AppContext) -> Result<Value> {
    read_auth_file(&context.paths.live_auth_path)?
        .ok_or_else(|| anyhow::anyhow!("~/.codex/auth.json not found"))
}

pub fn read_current_codex_auth_optional(context: &AppContext) -> Result<Option<Value>> {
    read_auth_file(&context.paths.live_auth_path)
}

pub fn read_auth_file(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read auth file {}", path.display()))?;
    let value = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    Ok(Some(value))
}

pub fn read_current_auth_status(context: &AppContext) -> Result<CurrentAuthStatus> {
    read_auth_status_for_path(&context.paths.live_auth_path)
}

pub fn read_auth_status_for_path(path: &Path) -> Result<CurrentAuthStatus> {
    if !path.exists() {
        return Ok(CurrentAuthStatus {
            available: false,
            account_id: None,
            email: None,
            plan_type: None,
            auth_mode: None,
            last_refresh: None,
            file_modified_at: None,
            fingerprint: None,
        });
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64);
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read auth file {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;

    let auth_mode = value
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let last_refresh = value
        .get("last_refresh")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let extracted = extract_auth(&value).ok();
    let account_id = extracted.as_ref().map(|auth| auth.account_id.clone());
    let email = extracted.as_ref().and_then(|auth| auth.email.clone());
    let plan_type = extracted.as_ref().and_then(|auth| auth.plan_type.clone());
    let fingerprint = Some(format!(
        "{}|{}|{}|{}",
        account_id.clone().unwrap_or_default(),
        last_refresh.clone().unwrap_or_default(),
        modified_at.unwrap_or_default(),
        auth_mode.clone().unwrap_or_default()
    ));

    Ok(CurrentAuthStatus {
        available: true,
        account_id,
        email,
        plan_type,
        auth_mode,
        last_refresh,
        file_modified_at: modified_at,
        fingerprint,
    })
}

pub fn write_active_codex_auth(context: &AppContext, auth_json: &Value) -> Result<()> {
    if let Some(parent) = context.paths.live_auth_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let serialized =
        serde_json::to_string_pretty(auth_json).context("failed to serialize auth.json")?;
    fs::write(&context.paths.live_auth_path, serialized).with_context(|| {
        format!(
            "failed to write live auth {}",
            context.paths.live_auth_path.display()
        )
    })?;
    set_private_permissions(&context.paths.live_auth_path);
    Ok(())
}

pub fn restore_active_codex_auth(context: &AppContext, backup: Option<Value>) -> Result<()> {
    match backup {
        Some(value) => write_active_codex_auth(context, &value),
        None => remove_active_codex_auth(context),
    }
}

pub fn remove_active_codex_auth(context: &AppContext) -> Result<()> {
    if context.paths.live_auth_path.exists() {
        fs::remove_file(&context.paths.live_auth_path).with_context(|| {
            format!(
                "failed to remove live auth {}",
                context.paths.live_auth_path.display()
            )
        })?;
    }
    Ok(())
}

pub fn normalize_imported_auth_json(auth_json: Value) -> Value {
    let Some(root) = auth_json.as_object() else {
        return auth_json;
    };

    if root.get("tokens").and_then(Value::as_object).is_some() {
        return auth_json;
    }

    let Some(access_token) = root.get("access_token").and_then(Value::as_str) else {
        return auth_json;
    };
    let Some(id_token) = root.get("id_token").and_then(Value::as_str) else {
        return auth_json;
    };

    let mut tokens = Map::new();
    tokens.insert(
        "access_token".to_string(),
        Value::String(access_token.to_string()),
    );
    tokens.insert("id_token".to_string(), Value::String(id_token.to_string()));
    if let Some(refresh_token) = root.get("refresh_token").and_then(Value::as_str) {
        tokens.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.to_string()),
        );
    }
    if let Some(account_id) = root.get("account_id").and_then(Value::as_str) {
        tokens.insert(
            "account_id".to_string(),
            Value::String(account_id.to_string()),
        );
    }

    let mut normalized = Map::new();
    normalized.insert(
        "auth_mode".to_string(),
        Value::String(
            root.get("auth_mode")
                .and_then(Value::as_str)
                .unwrap_or("chatgpt")
                .to_string(),
        ),
    );
    normalized.insert("tokens".to_string(), Value::Object(tokens));

    if let Some(last_refresh) = root.get("last_refresh") {
        normalized.insert("last_refresh".to_string(), last_refresh.clone());
    }

    Value::Object(normalized)
}

pub fn extract_auth(auth_json: &Value) -> Result<ExtractedAuth> {
    let mode = auth_json
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    let tokens = auth_token_object(auth_json);
    let tokens = match tokens {
        Some(tokens) => tokens,
        None => {
            if !mode.is_empty() && mode != "chatgpt" && mode != "chatgpt_auth_tokens" {
                anyhow::bail!(
                    "current account is not using ChatGPT sign-in mode; run codex login first"
                );
            }
            anyhow::bail!("no ChatGPT sign-in token detected; run codex login first");
        }
    };

    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("auth.json is missing access_token"))?
        .to_string();
    let id_token = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("auth.json is missing id_token"))?;

    let mut account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut email = None;
    let mut plan_type = None;

    if let Ok(claims) = decode_jwt_payload(id_token) {
        email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let auth_claim = claims.get("https://api.openai.com/auth");
        if account_id.is_none() {
            account_id = auth_claim
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        plan_type = auth_claim
            .and_then(|value| value.get("chatgpt_plan_type"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }

    Ok(ExtractedAuth {
        account_id: account_id
            .ok_or_else(|| anyhow::anyhow!("failed to read chatgpt_account_id from auth.json"))?,
        access_token,
        email,
        plan_type,
    })
}

pub fn current_auth_account_id(context: &AppContext) -> Option<String> {
    read_current_codex_auth(context)
        .ok()
        .and_then(|value| extract_auth(&value).ok())
        .map(|auth| auth.account_id)
}

pub async fn refresh_chatgpt_auth_tokens(auth_json: &Value) -> Result<Value> {
    let tokens = auth_token_object(auth_json)
        .ok_or_else(|| anyhow::anyhow!("auth.json is missing tokens"))?;
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("auth.json is missing refresh_token"))?;
    let id_token = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("auth.json is missing id_token"))?;

    let claims = decode_jwt_payload(id_token)?;
    let issuer = claims
        .get("iss")
        .and_then(Value::as_str)
        .unwrap_or("https://auth.openai.com")
        .trim_end_matches('/')
        .to_string();
    let token_url = format!("{issuer}/oauth/token");

    let mut form_pairs: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if let Some(client_id) = extract_client_id_from_claims(&claims) {
        form_pairs.push(("client_id", client_id));
    }

    let client = reqwest::Client::builder()
        .user_agent("codex-pool/0.1")
        .build()
        .context("failed to build HTTP client")?;
    let response = client
        .post(&token_url)
        .form(&form_pairs)
        .send()
        .await
        .with_context(|| format!("failed to refresh login token {token_url}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "failed to refresh login token {token_url} -> {status}: {}",
            truncate_for_error(&body, 140)
        );
    }

    let refreshed: RefreshedTokenPayload = response
        .json()
        .await
        .context("failed to parse token refresh response")?;

    let mut updated = auth_json.clone();
    let root = updated
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("auth.json root is not an object"))?;
    let tokens = root
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("auth.json is missing tokens"))?;
    tokens.insert(
        "access_token".to_string(),
        Value::String(refreshed.access_token),
    );
    tokens.insert("id_token".to_string(), Value::String(refreshed.id_token));
    if let Some(refresh_token) = refreshed.refresh_token {
        tokens.insert("refresh_token".to_string(), Value::String(refresh_token));
    }

    Ok(updated)
}

fn auth_token_object(auth_json: &Value) -> Option<&Map<String, Value>> {
    auth_json
        .get("tokens")
        .and_then(Value::as_object)
        .or_else(|| {
            let root = auth_json.as_object()?;
            if root.contains_key("access_token") && root.contains_key("id_token") {
                Some(root)
            } else {
                None
            }
        })
}

pub fn decode_jwt_payload(token: &str) -> Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("invalid id_token format"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            let remainder = payload.len() % 4;
            let padded = if remainder == 0 {
                payload.to_string()
            } else {
                format!("{payload}{}", "=".repeat(4 - remainder))
            };
            URL_SAFE.decode(padded)
        })
        .context("failed to decode id_token")?;

    serde_json::from_slice(&decoded).context("failed to parse id_token payload")
}

#[derive(Debug, serde::Deserialize)]
struct RefreshedTokenPayload {
    access_token: String,
    id_token: String,
    refresh_token: Option<String>,
}

fn extract_client_id_from_claims(claims: &Value) -> Option<String> {
    let aud = claims.get("aud")?;
    match aud {
        Value::String(value) if !value.is_empty() => Some(value.to_string()),
        Value::Array(values) => values.iter().find_map(|item| {
            item.as_str()
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::decode_jwt_payload;
    use super::extract_auth;
    use super::normalize_imported_auth_json;

    #[test]
    fn normalizes_flat_auth_json() {
        let raw = json!({
            "access_token": "access",
            "id_token": "header.eyJlbWFpbCI6ImRlbW9AZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEifX0.sig",
            "refresh_token": "refresh"
        });
        let normalized = normalize_imported_auth_json(raw);
        assert!(normalized.get("tokens").is_some());
    }

    #[test]
    fn decodes_jwt_payload() {
        let payload = decode_jwt_payload(
            "header.eyJlbWFpbCI6ImRlbW9AZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ.sig",
        )
        .expect("jwt should decode");
        assert_eq!(payload["email"], "demo@example.com");
    }

    #[test]
    fn extracts_nested_auth() {
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "access",
                "refresh_token": "refresh",
                "id_token": "header.eyJlbWFpbCI6ImRlbW9AZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ.sig"
            }
        });

        let extracted = extract_auth(&auth).expect("auth should parse");
        assert_eq!(extracted.account_id, "acc-1");
        assert_eq!(extracted.email.as_deref(), Some("demo@example.com"));
        assert_eq!(extracted.plan_type.as_deref(), Some("pro"));
    }
}
