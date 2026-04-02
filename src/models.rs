use std::collections::BTreeMap;
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
    #[serde(default)]
    pub proxy: ProxyConfig,
}

pub fn default_watch_interval_seconds() -> u64 {
    60
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: default_store_version(),
            default_watch_interval_seconds: default_watch_interval_seconds(),
            proxy: ProxyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_listen")]
    pub listen: String,
    #[serde(default = "default_proxy_api_key")]
    pub api_key: String,
    #[serde(default = "default_proxy_default_cwd")]
    pub default_cwd: String,
    #[serde(default = "default_proxy_default_model")]
    pub default_model: String,
    #[serde(default = "default_proxy_sandbox")]
    pub sandbox: String,
    #[serde(default = "default_proxy_approval_policy")]
    pub approval_policy: String,
    #[serde(default = "default_proxy_usage_refresh_interval_seconds")]
    pub usage_refresh_interval_seconds: u64,
    #[serde(default = "default_proxy_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_proxy_max_inflight_per_account")]
    pub max_inflight_per_account: usize,
    #[serde(default)]
    pub model_aliases: BTreeMap<String, String>,
}

pub fn default_proxy_listen() -> String {
    "127.0.0.1:4141".to_string()
}

pub fn default_proxy_api_key() -> String {
    "codex-pool-local".to_string()
}

pub fn default_proxy_default_cwd() -> String {
    ".".to_string()
}

pub fn default_proxy_default_model() -> String {
    "gpt-5.4".to_string()
}

pub fn default_proxy_sandbox() -> String {
    "workspace-write".to_string()
}

pub fn default_proxy_approval_policy() -> String {
    "never".to_string()
}

pub fn default_proxy_usage_refresh_interval_seconds() -> u64 {
    60
}

pub fn default_proxy_max_concurrent_requests() -> usize {
    8
}

pub fn default_proxy_max_inflight_per_account() -> usize {
    1
}

impl Default for ProxyConfig {
    fn default() -> Self {
        let default_model = default_proxy_default_model();
        let mut model_aliases = BTreeMap::new();
        model_aliases.insert("codex".to_string(), default_model.clone());
        Self {
            listen: default_proxy_listen(),
            api_key: default_proxy_api_key(),
            default_cwd: default_proxy_default_cwd(),
            default_model,
            sandbox: default_proxy_sandbox(),
            approval_policy: default_proxy_approval_policy(),
            usage_refresh_interval_seconds: default_proxy_usage_refresh_interval_seconds(),
            max_concurrent_requests: default_proxy_max_concurrent_requests(),
            max_inflight_per_account: default_proxy_max_inflight_per_account(),
            model_aliases,
        }
    }
}

impl ProxyConfig {
    pub fn normalize(&mut self, cwd: &str) {
        if self.default_cwd.trim().is_empty() || self.default_cwd == "." {
            self.default_cwd = cwd.to_string();
        }
        if self.default_model.trim().is_empty() {
            self.default_model = default_proxy_default_model();
        }
        if self.listen.trim().is_empty() {
            self.listen = default_proxy_listen();
        }
        if self.api_key.trim().is_empty() {
            self.api_key = default_proxy_api_key();
        }
        if self.sandbox.trim().is_empty() {
            self.sandbox = default_proxy_sandbox();
        }
        if self.approval_policy.trim().is_empty() {
            self.approval_policy = default_proxy_approval_policy();
        }
        if self.usage_refresh_interval_seconds == 0 {
            self.usage_refresh_interval_seconds = default_proxy_usage_refresh_interval_seconds();
        }
        if self.max_concurrent_requests == 0 {
            self.max_concurrent_requests = default_proxy_max_concurrent_requests();
        }
        if self.max_inflight_per_account == 0 {
            self.max_inflight_per_account = default_proxy_max_inflight_per_account();
        }
        self.model_aliases
            .entry("codex".to_string())
            .or_insert_with(|| self.default_model.clone());
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
