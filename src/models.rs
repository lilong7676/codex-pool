use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsStore {
    #[serde(default = "default_store_version")]
    pub version: u8,
    #[serde(default)]
    pub accounts: Vec<StoredAccount>,
}

fn default_store_version() -> u8 {
    1
}

impl Default for AccountsStore {
    fn default() -> Self {
        Self {
            version: default_store_version(),
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAccount {
    pub id: String,
    pub label: String,
    pub email: Option<String>,
    pub account_id: String,
    pub plan_type: Option<String>,
    pub auth_json: Value,
    pub added_at: i64,
    pub updated_at: i64,
    pub usage: Option<UsageSnapshot>,
    pub usage_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub fetched_at: i64,
    pub plan_type: Option<String>,
    pub five_hour: Option<UsageWindow>,
    pub one_week: Option<UsageWindow>,
    pub credits: Option<CreditSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub used_percent: f64,
    pub window_seconds: i64,
    pub reset_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Healthy,
    Expired,
    ReauthRequired,
    WorkspaceRemoved,
    UnknownError,
}

impl fmt::Display for AccountStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Expired => write!(f, "expired"),
            Self::ReauthRequired => write!(f, "reauth_required"),
            Self::WorkspaceRemoved => write!(f, "workspace_removed"),
            Self::UnknownError => write!(f, "unknown_error"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub id: String,
    pub label: String,
    pub email: Option<String>,
    pub account_id: String,
    pub plan_type: Option<String>,
    pub added_at: i64,
    pub updated_at: i64,
    pub usage: Option<UsageSnapshot>,
    pub usage_error: Option<String>,
    pub is_current: bool,
    pub status: AccountStatus,
}

impl StoredAccount {
    pub fn to_summary(&self, current_account_id: Option<&str>) -> AccountSummary {
        AccountSummary {
            id: self.id.clone(),
            label: self.label.clone(),
            email: self.email.clone(),
            account_id: self.account_id.clone(),
            plan_type: self.plan_type.clone(),
            added_at: self.added_at,
            updated_at: self.updated_at,
            usage: self.usage.clone(),
            usage_error: self.usage_error.clone(),
            is_current: current_account_id
                .map(|current| current == self.account_id)
                .unwrap_or(false),
            status: AccountStatus::classify(self.usage.as_ref(), self.usage_error.as_deref()),
        }
    }
}

impl AccountStatus {
    pub fn classify(usage: Option<&UsageSnapshot>, usage_error: Option<&str>) -> Self {
        let Some(raw_error) = usage_error else {
            return if usage.is_some() {
                Self::Healthy
            } else {
                Self::UnknownError
            };
        };

        let normalized = raw_error.to_ascii_lowercase();
        if normalized.contains("deactivated_workspace") {
            return Self::WorkspaceRemoved;
        }
        if normalized.contains("provided authentication token is expired")
            || normalized.contains("your refresh token has already been used")
            || normalized.contains("please try signing in again")
            || normalized.contains("token is expired")
            || normalized.contains("authorization expired")
        {
            return Self::Expired;
        }
        if normalized.contains("run codex login first")
            || normalized.contains("auth.json 缺少")
            || normalized.contains("无法从 auth.json 识别")
            || normalized.contains("not using chatgpt sign-in")
        {
            return Self::ReauthRequired;
        }
        Self::UnknownError
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentAuthStatus {
    pub available: bool,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub auth_mode: Option<String>,
    pub last_refresh: Option<String>,
    pub file_modified_at: Option<i64>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractedAuth {
    pub account_id: String,
    pub access_token: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_store_version")]
    pub version: u8,
    #[serde(default = "default_watch_interval_seconds")]
    pub default_watch_interval_seconds: u64,
}

pub fn default_watch_interval_seconds() -> u64 {
    60
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: default_store_version(),
            default_watch_interval_seconds: default_watch_interval_seconds(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportStats {
    pub imported: usize,
    pub updated: usize,
}

impl ImportStats {
    pub fn total_changed(&self) -> usize {
        self.imported + self.updated
    }
}
