use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use crate::models::AccountStatus;
use crate::models::AccountSummary;
use crate::models::AccountsStore;
use crate::models::StoredAccount;
use crate::ranking::compare_accounts_by_remaining;
use crate::utils::now_unix_seconds;

const DEFAULT_COOLDOWN_SECONDS: i64 = 30;

#[derive(Debug, Clone)]
pub struct SchedulerSnapshot {
    pub summary: AccountSummary,
    pub inflight: usize,
    pub cooldown_until: Option<i64>,
}

#[derive(Debug)]
struct ScheduledAccountState {
    stored: StoredAccount,
    summary: AccountSummary,
    inflight: usize,
    cooldown_until: Option<i64>,
}

#[derive(Debug)]
struct SchedulerState {
    accounts: Vec<ScheduledAccountState>,
}

#[derive(Debug)]
pub struct Scheduler {
    state: Mutex<SchedulerState>,
    semaphore: Arc<Semaphore>,
    max_inflight_per_account: usize,
}

pub struct SchedulerLease {
    scheduler: Arc<Scheduler>,
    account_id: String,
    pub account: StoredAccount,
    pub summary: AccountSummary,
    _permit: OwnedSemaphorePermit,
}

impl Scheduler {
    pub fn new(
        store: &AccountsStore,
        summaries: &[AccountSummary],
        max_concurrent_requests: usize,
        max_inflight_per_account: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SchedulerState {
                accounts: build_accounts(store, summaries, None),
            }),
            semaphore: Arc::new(Semaphore::new(max_concurrent_requests.max(1))),
            max_inflight_per_account: max_inflight_per_account.max(1),
        })
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<SchedulerLease> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("proxy scheduler is shutting down"))?;

        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy scheduler lock poisoned"))?;
        let now = now_unix_seconds();
        let mut candidates = state
            .accounts
            .iter()
            .filter(|account| account.summary.status != AccountStatus::Expired)
            .filter(|account| account.summary.status != AccountStatus::WorkspaceRemoved)
            .filter(|account| account.inflight < self.max_inflight_per_account)
            .filter(|account| {
                account
                    .cooldown_until
                    .map(|value| value <= now)
                    .unwrap_or(true)
            })
            .map(|account| {
                (
                    account.stored.account_id.clone(),
                    account.inflight,
                    account.summary.clone(),
                )
            })
            .collect::<Vec<_>>();

        candidates.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| compare_accounts_by_remaining(&left.2, &right.2))
        });

        let Some((account_id, _, _)) = candidates.into_iter().next() else {
            return Err(anyhow::anyhow!("no proxy account available"));
        };

        let selected = state
            .accounts
            .iter_mut()
            .find(|account| account.stored.account_id == account_id)
            .ok_or_else(|| anyhow::anyhow!("selected account disappeared"))?;
        selected.inflight += 1;

        Ok(SchedulerLease {
            scheduler: Arc::clone(self),
            account_id: selected.stored.account_id.clone(),
            account: selected.stored.clone(),
            summary: selected.summary.clone(),
            _permit: permit,
        })
    }

    pub fn replace_accounts(
        &self,
        store: &AccountsStore,
        summaries: &[AccountSummary],
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy scheduler lock poisoned"))?;
        let previous = state
            .accounts
            .iter()
            .map(|account| {
                (
                    account.stored.account_id.clone(),
                    (account.inflight, account.cooldown_until),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        state.accounts = build_accounts(store, summaries, Some(&previous));
        Ok(())
    }

    pub fn cooldown_account(&self, account_id: &str, seconds: Option<i64>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy scheduler lock poisoned"))?;
        if let Some(account) = state
            .accounts
            .iter_mut()
            .find(|account| account.stored.account_id == account_id)
        {
            account.cooldown_until =
                Some(now_unix_seconds() + seconds.unwrap_or(DEFAULT_COOLDOWN_SECONDS));
        }
        Ok(())
    }

    pub fn snapshots(&self) -> Result<Vec<SchedulerSnapshot>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy scheduler lock poisoned"))?;
        let mut snapshots = state
            .accounts
            .iter()
            .map(|account| SchedulerSnapshot {
                summary: account.summary.clone(),
                inflight: account.inflight,
                cooldown_until: account.cooldown_until,
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.inflight
                .cmp(&right.inflight)
                .then_with(|| compare_accounts_by_remaining(&left.summary, &right.summary))
        });
        Ok(snapshots)
    }

    fn release_account(&self, account_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(account) = state
                .accounts
                .iter_mut()
                .find(|account| account.stored.account_id == account_id)
            {
                account.inflight = account.inflight.saturating_sub(1);
            }
        }
    }
}

impl Drop for SchedulerLease {
    fn drop(&mut self) {
        self.scheduler.release_account(&self.account_id);
    }
}

fn build_accounts(
    store: &AccountsStore,
    summaries: &[AccountSummary],
    previous: Option<&std::collections::HashMap<String, (usize, Option<i64>)>>,
) -> Vec<ScheduledAccountState> {
    store
        .accounts
        .iter()
        .filter_map(|stored| {
            let summary = summaries
                .iter()
                .find(|summary| summary.account_id == stored.account_id)?;
            let (inflight, cooldown_until) = previous
                .and_then(|map| map.get(&stored.account_id).copied())
                .unwrap_or((0, None));
            Some(ScheduledAccountState {
                stored: stored.clone(),
                summary: summary.clone(),
                inflight,
                cooldown_until,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::models::UsageSnapshot;
    use crate::models::UsageWindow;
    use crate::store;

    use super::*;

    fn summary(
        account_id: &str,
        label: &str,
        one_week_used: f64,
        five_hour_used: f64,
        status: AccountStatus,
    ) -> AccountSummary {
        AccountSummary {
            id: label.to_string(),
            label: label.to_string(),
            email: None,
            account_id: account_id.to_string(),
            plan_type: Some("pro".to_string()),
            added_at: 0,
            updated_at: 0,
            usage: Some(UsageSnapshot {
                fetched_at: 0,
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
            }),
            usage_error: None,
            is_current: false,
            status,
        }
    }

    fn build_store(account_ids: &[(&str, &str)]) -> AccountsStore {
        let mut store_value = AccountsStore::default();
        for (account_id, label) in account_ids {
            let auth = serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": format!("access-{account_id}"),
                    "refresh_token": format!("refresh-{account_id}"),
                    "account_id": *account_id,
                    "id_token": "header.eyJlbWFpbCI6ImRlbW9AZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ.sig"
                }
            });
            let _ = store::upsert_auth_account(
                &mut store_value,
                auth,
                Some((*label).to_string()),
                None,
                None,
            )
            .expect("account should upsert");
        }
        store_value
    }

    #[tokio::test]
    async fn prefers_lower_inflight_then_remaining() {
        let store = build_store(&[("acc-a", "A"), ("acc-b", "B")]);
        let summaries = vec![
            summary("acc-a", "A", 10.0, 10.0, AccountStatus::Healthy),
            summary("acc-b", "B", 20.0, 20.0, AccountStatus::Healthy),
        ];
        let scheduler = Scheduler::new(&store, &summaries, 4, 2);
        let _first = scheduler.acquire().await.expect("first lease should work");
        let second = scheduler.acquire().await.expect("second lease should work");
        assert_eq!(second.account.account_id, "acc-b");
    }

    #[tokio::test]
    async fn skips_cooldown_and_excluded_accounts() {
        let store = build_store(&[("acc-a", "A"), ("acc-b", "B"), ("acc-c", "C")]);
        let summaries = vec![
            summary("acc-a", "A", 10.0, 10.0, AccountStatus::Expired),
            summary("acc-b", "B", 15.0, 15.0, AccountStatus::Healthy),
            summary("acc-c", "C", 20.0, 20.0, AccountStatus::Healthy),
        ];
        let scheduler = Scheduler::new(&store, &summaries, 4, 1);
        scheduler
            .cooldown_account("acc-c", Some(60))
            .expect("cooldown should work");
        let lease = scheduler.acquire().await.expect("lease should work");
        assert_eq!(lease.account.account_id, "acc-b");
    }
}
