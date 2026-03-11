use std::error::Error as StdError;
use std::fs;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;

use crate::context::AppContext;
use crate::context::MockUsageResponse;
use crate::models::CreditSnapshot;
use crate::models::UsageSnapshot;
use crate::models::UsageWindow;
use crate::utils::now_unix_seconds;
use crate::utils::truncate_for_error;

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com";
const CODEX_USAGE_PATH: &str = "/api/codex/usage";
const WHAM_USAGE_PATH: &str = "/wham/usage";
const BACKEND_API_PREFIX: &str = "/backend-api";

#[derive(Debug, Deserialize)]
struct UsageApiResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimitDetails>,
    additional_rate_limits: Option<Vec<AdditionalRateLimitDetails>>,
    credits: Option<CreditDetails>,
}

#[derive(Debug, Deserialize)]
struct RateLimitDetails {
    primary_window: Option<UsageWindowRaw>,
    secondary_window: Option<UsageWindowRaw>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimitDetails {
    rate_limit: Option<RateLimitDetails>,
}

#[derive(Debug, Clone, Deserialize)]
struct UsageWindowRaw {
    used_percent: f64,
    limit_window_seconds: i64,
    reset_at: i64,
}

#[derive(Debug, Deserialize)]
struct CreditDetails {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

pub async fn fetch_usage_snapshot(
    context: &AppContext,
    access_token: &str,
    account_id: &str,
) -> Result<UsageSnapshot> {
    if let Some(mock) = context.test_hooks.mock_usage.get(account_id) {
        return match mock {
            MockUsageResponse::Snapshot(snapshot) => Ok(snapshot.clone()),
            MockUsageResponse::Error(error) => Err(anyhow::anyhow!(error.clone())),
        };
    }

    let usage_urls = resolve_usage_urls(context);
    let client = reqwest::Client::builder()
        .user_agent("codex-pool/0.1")
        .timeout(Duration::from_secs(18))
        .build()
        .context("failed to create HTTP client")?;

    let mut errors = Vec::new();
    for usage_url in usage_urls {
        let response = match client
            .get(&usage_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("ChatGPT-Account-Id", account_id)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                errors.push(format!("{usage_url} -> {}", format_reqwest_error(&error)));
                continue;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            errors.push(format!(
                "{usage_url} -> {status}: {}",
                truncate_for_error(&body, 140)
            ));
            continue;
        }

        let payload: UsageApiResponse = response
            .json()
            .await
            .with_context(|| format!("failed to parse response from {usage_url}"))?;
        return Ok(map_usage_payload(payload));
    }

    let preview = errors
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    if errors.is_empty() {
        anyhow::bail!("failed to query usage endpoint: no candidate URL succeeded");
    } else if errors.len() > 2 {
        anyhow::bail!(
            "failed to query usage endpoint: {preview} | and {} more failures",
            errors.len() - 2
        );
    } else {
        anyhow::bail!("failed to query usage endpoint: {preview}");
    }
}

pub fn resolve_usage_urls(context: &AppContext) -> Vec<String> {
    let normalized = resolve_chatgpt_base_origin(context);
    let mut candidates = Vec::new();

    if let Some(origin) = normalized.strip_suffix(BACKEND_API_PREFIX) {
        candidates.push(format!("{normalized}{WHAM_USAGE_PATH}"));
        candidates.push(format!("{origin}{BACKEND_API_PREFIX}{WHAM_USAGE_PATH}"));
        candidates.push(format!("{origin}{CODEX_USAGE_PATH}"));
    } else {
        candidates.push(format!("{normalized}{BACKEND_API_PREFIX}{WHAM_USAGE_PATH}"));
        candidates.push(format!("{normalized}{WHAM_USAGE_PATH}"));
        candidates.push(format!("{normalized}{CODEX_USAGE_PATH}"));
    }

    candidates.push("https://chatgpt.com/backend-api/wham/usage".to_string());
    candidates.push(format!("https://chatgpt.com{CODEX_USAGE_PATH}"));
    let mut deduped = Vec::new();
    for candidate in candidates {
        if !deduped.iter().any(|existing| existing == &candidate) {
            deduped.push(candidate);
        }
    }
    deduped
}

fn resolve_chatgpt_base_origin(context: &AppContext) -> String {
    read_chatgpt_base_url_from_config(context)
        .unwrap_or_else(|| DEFAULT_CHATGPT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn read_chatgpt_base_url_from_config(context: &AppContext) -> Option<String> {
    let contents = fs::read_to_string(&context.paths.codex_config_path).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("chatgpt_base_url") {
            continue;
        }
        let (_, value) = trimmed.split_once('=')?;
        let cleaned = value.trim().trim_matches('"').trim_matches('\'');
        if !cleaned.is_empty() {
            return Some(cleaned.to_string());
        }
    }
    None
}

pub fn should_retry_with_token_refresh(fetch_result: &Result<UsageSnapshot>) -> bool {
    match fetch_result {
        Ok(snapshot) => snapshot.plan_type.is_none(),
        Err(error) => {
            let normalized = error.to_string().to_ascii_lowercase();
            normalized.contains("401")
                || normalized.contains("unauthorized")
                || normalized.contains("invalid_token")
                || normalized.contains("deactivated_workspace")
        }
    }
}

pub fn normalize_usage_error_message(raw_error: &str) -> String {
    let normalized = raw_error.to_ascii_lowercase();
    if normalized.contains("deactivated_workspace") {
        return "deactivated_workspace: account was removed from the team workspace".to_string();
    }
    if normalized.contains("provided authentication token is expired")
        || normalized.contains("your refresh token has already been used")
        || normalized.contains("please try signing in again")
        || normalized.contains("token is expired")
    {
        return "authorization expired, please re-auth this account".to_string();
    }
    raw_error.to_string()
}

fn map_usage_payload(payload: UsageApiResponse) -> UsageSnapshot {
    let mut windows = Vec::new();

    if let Some(rate_limit) = payload.rate_limit {
        if let Some(primary) = rate_limit.primary_window {
            windows.push(primary);
        }
        if let Some(secondary) = rate_limit.secondary_window {
            windows.push(secondary);
        }
    }

    if let Some(additional) = payload.additional_rate_limits {
        for limit in additional {
            if let Some(rate_limit) = limit.rate_limit {
                if let Some(primary) = rate_limit.primary_window {
                    windows.push(primary);
                }
                if let Some(secondary) = rate_limit.secondary_window {
                    windows.push(secondary);
                }
            }
        }
    }

    let five_hour = pick_nearest_window(&windows, 5 * 60 * 60).map(to_usage_window);
    let one_week = pick_nearest_window(&windows, 7 * 24 * 60 * 60).map(to_usage_window);

    UsageSnapshot {
        fetched_at: now_unix_seconds(),
        plan_type: payload.plan_type,
        five_hour,
        one_week,
        credits: payload.credits.map(|credit| CreditSnapshot {
            has_credits: credit.has_credits,
            unlimited: credit.unlimited,
            balance: credit.balance,
        }),
    }
}

fn pick_nearest_window(windows: &[UsageWindowRaw], target_seconds: i64) -> Option<UsageWindowRaw> {
    windows
        .iter()
        .min_by_key(|window| (window.limit_window_seconds - target_seconds).abs())
        .cloned()
}

fn to_usage_window(window: UsageWindowRaw) -> UsageWindow {
    UsageWindow {
        used_percent: window.used_percent,
        window_seconds: window.limit_window_seconds,
        reset_at: Some(window.reset_at),
    }
}

fn format_reqwest_error(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(next) = source {
        let text = next.to_string();
        if !parts.iter().any(|item| item == &text) {
            parts.push(text);
        }
        source = next.source();
    }
    parts.join(" -> ")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::context::AppContext;
    use crate::context::AppPaths;

    use super::resolve_usage_urls;

    #[test]
    fn usage_urls_prefer_custom_base_url() {
        let temp = TempDir::new().expect("tempdir should exist");
        let paths = AppPaths::for_home(temp.path().to_path_buf());
        fs::create_dir_all(paths.codex_config_path.parent().expect("parent exists"))
            .expect("codex dir should exist");
        fs::write(
            &paths.codex_config_path,
            "chatgpt_base_url = \"https://chatgpt.com/backend-api\"\n",
        )
        .expect("config should be written");
        let context = AppContext::with_paths(paths);

        let urls = resolve_usage_urls(&context);
        assert_eq!(urls[0], "https://chatgpt.com/backend-api/wham/usage");
    }
}
