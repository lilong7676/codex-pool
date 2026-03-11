use std::cmp::Ordering;

use crate::models::AccountStatus;
use crate::models::AccountSummary;
use crate::models::UsageWindow;

const UNKNOWN_REMAINING: f64 = -1.0;

fn window_remaining_percent(window: Option<&UsageWindow>) -> f64 {
    let Some(window) = window else {
        return UNKNOWN_REMAINING;
    };
    (100.0 - window.used_percent).clamp(0.0, 100.0)
}

fn account_remaining_score(account: &AccountSummary) -> (f64, f64) {
    (
        window_remaining_percent(
            account
                .usage
                .as_ref()
                .and_then(|usage| usage.one_week.as_ref()),
        ),
        window_remaining_percent(
            account
                .usage
                .as_ref()
                .and_then(|usage| usage.five_hour.as_ref()),
        ),
    )
}

pub fn compare_accounts_by_remaining(a: &AccountSummary, b: &AccountSummary) -> Ordering {
    let left = account_remaining_score(a);
    let right = account_remaining_score(b);

    right
        .0
        .partial_cmp(&left.0)
        .unwrap_or(Ordering::Equal)
        .then_with(|| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal))
        .then_with(|| match (a.is_current, b.is_current) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        })
        .then_with(|| a.label.cmp(&b.label))
}

pub fn sort_accounts_by_remaining(mut accounts: Vec<AccountSummary>) -> Vec<AccountSummary> {
    accounts.sort_by(compare_accounts_by_remaining);
    accounts
}

pub fn pick_best_account(accounts: &[AccountSummary]) -> Option<AccountSummary> {
    let mut candidates = accounts
        .iter()
        .filter(|account| {
            account.status != AccountStatus::Expired
                && account.status != AccountStatus::WorkspaceRemoved
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(compare_accounts_by_remaining);
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use crate::models::AccountStatus;
    use crate::models::AccountSummary;
    use crate::models::UsageSnapshot;
    use crate::models::UsageWindow;

    use super::pick_best_account;

    fn summary(
        label: &str,
        account_id: &str,
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
                    window_seconds: 18000,
                    reset_at: Some(1),
                }),
                one_week: Some(UsageWindow {
                    used_percent: one_week_used,
                    window_seconds: 604800,
                    reset_at: Some(1),
                }),
                credits: None,
            }),
            usage_error: None,
            is_current: false,
            status,
        }
    }

    #[test]
    fn prefers_more_one_week_remaining_before_five_hour() {
        let accounts = vec![
            summary("A", "acc-a", 20.0, 80.0, AccountStatus::Healthy),
            summary("B", "acc-b", 10.0, 95.0, AccountStatus::Healthy),
        ];

        let selected = pick_best_account(&accounts).expect("should pick account");
        assert_eq!(selected.account_id, "acc-b");
    }

    #[test]
    fn skips_expired_accounts() {
        let accounts = vec![
            summary("A", "acc-a", 1.0, 1.0, AccountStatus::Expired),
            summary("B", "acc-b", 20.0, 20.0, AccountStatus::Healthy),
        ];

        let selected = pick_best_account(&accounts).expect("should pick healthy account");
        assert_eq!(selected.account_id, "acc-b");
    }
}
