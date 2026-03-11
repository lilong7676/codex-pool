use comfy_table::Cell;
use comfy_table::ContentArrangement;
use comfy_table::Table;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;

use crate::models::AccountSummary;
use crate::models::UsageWindow;
use crate::utils::format_percent;
use crate::utils::format_timestamp;
use crate::utils::short_account;

pub fn render_account_table(accounts: &[AccountSummary]) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "label",
            "account",
            "plan",
            "status",
            "5h used",
            "5h remain",
            "5h reset",
            "1w used",
            "1w remain",
            "1w reset",
            "current",
            "last refreshed",
        ]);

    for account in accounts {
        let five_hour = account
            .usage
            .as_ref()
            .and_then(|usage| usage.five_hour.as_ref());
        let one_week = account
            .usage
            .as_ref()
            .and_then(|usage| usage.one_week.as_ref());
        table.add_row(vec![
            Cell::new(&account.label),
            Cell::new(short_account(&account.account_id)),
            Cell::new(
                account
                    .plan_type
                    .clone()
                    .unwrap_or_else(|| "--".to_string()),
            ),
            Cell::new(account.status.to_string()),
            Cell::new(format_percent(five_hour.map(|window| window.used_percent))),
            Cell::new(format_remaining(five_hour)),
            Cell::new(format_timestamp(
                five_hour.and_then(|window| window.reset_at),
            )),
            Cell::new(format_percent(one_week.map(|window| window.used_percent))),
            Cell::new(format_remaining(one_week)),
            Cell::new(format_timestamp(
                one_week.and_then(|window| window.reset_at),
            )),
            Cell::new(if account.is_current { "yes" } else { "no" }),
            Cell::new(format_timestamp(
                account.usage.as_ref().map(|usage| usage.fetched_at),
            )),
        ]);
    }

    let mut rendered = table.to_string();
    let errors = accounts
        .iter()
        .filter_map(|account| {
            account.usage_error.as_ref().map(|error| {
                format!(
                    "{} ({}) -> {}",
                    account.label,
                    short_account(&account.account_id),
                    error
                )
            })
        })
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        rendered.push_str("\n\nerrors:\n");
        for error in errors {
            rendered.push_str(&format!("- {error}\n"));
        }
    }

    rendered
}

fn format_remaining(window: Option<&UsageWindow>) -> String {
    format_percent(window.map(|window| 100.0 - window.used_percent))
}
