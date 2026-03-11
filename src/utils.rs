use std::fs;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

pub fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

pub fn short_account(account_id: &str) -> String {
    account_id.chars().take(8).collect()
}

pub fn truncate_for_error(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        value.to_string()
    } else {
        format!("{}...", &value[..max_len])
    }
}

pub fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

pub fn format_timestamp(value: Option<i64>) -> String {
    let Some(value) = value else {
        return "--".to_string();
    };

    let Ok(offset) = UtcOffset::current_local_offset() else {
        return OffsetDateTime::from_unix_timestamp(value)
            .ok()
            .and_then(|time| time.format(&Rfc3339).ok())
            .unwrap_or_else(|| "--".to_string());
    };

    OffsetDateTime::from_unix_timestamp(value)
        .ok()
        .map(|time| time.to_offset(offset))
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| "--".to_string())
}

pub fn format_percent(value: Option<f64>) -> String {
    value
        .map(|percent| format!("{percent:.1}%"))
        .unwrap_or_else(|| "--".to_string())
}
