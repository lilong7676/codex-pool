use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::extract_auth;
use crate::auth::normalize_imported_auth_json;
use crate::context::AppContext;
use crate::models::AccountsStore;
use crate::models::ImportStats;
use crate::models::StoredAccount;
use crate::models::UsageSnapshot;
use crate::utils::now_unix_seconds;
use crate::utils::set_private_permissions;
use crate::utils::short_account;

pub fn load_store(context: &AppContext) -> Result<AccountsStore> {
    load_store_from_path(&context.paths.store_path)
}

pub fn save_store(context: &AppContext, store: &AccountsStore) -> Result<()> {
    save_store_to_path(&context.paths.store_path, store)
}

pub fn load_store_from_path(path: &Path) -> Result<AccountsStore> {
    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read account store {}", path.display()))?;
    match serde_json::from_str::<AccountsStore>(&raw) {
        Ok(store) => Ok(store),
        Err(primary_err) => {
            let mut stream = serde_json::Deserializer::from_str(&raw).into_iter::<AccountsStore>();
            if let Some(Ok(recovered)) = stream.next() {
                let _ = write_store_file(path, &recovered);
                return Ok(recovered);
            }

            let fallback = AccountsStore::default();
            write_store_file(path, &fallback).with_context(|| {
                format!(
                    "invalid account store {} and failed to repair: {primary_err}",
                    path.display()
                )
            })?;
            Ok(fallback)
        }
    }
}

pub fn save_store_to_path(path: &Path, store: &AccountsStore) -> Result<()> {
    write_store_file(path, store)
}

pub fn upsert_auth_account(
    store: &mut AccountsStore,
    auth_json: Value,
    label: Option<String>,
    usage: Option<UsageSnapshot>,
    usage_error: Option<String>,
) -> Result<(StoredAccount, bool)> {
    let auth_json = normalize_imported_auth_json(auth_json);
    let extracted = extract_auth(&auth_json)?;
    let now = now_unix_seconds();
    let resolved_label = normalize_custom_label(label).unwrap_or_else(|| {
        fallback_account_label(extracted.email.as_deref(), &extracted.account_id)
    });

    if let Some(existing) = store
        .accounts
        .iter_mut()
        .find(|account| account.account_id == extracted.account_id)
    {
        existing.label = resolved_label;
        existing.email = extracted.email;
        existing.plan_type = usage
            .as_ref()
            .and_then(|snapshot| snapshot.plan_type.clone())
            .or(extracted.plan_type)
            .or(existing.plan_type.clone());
        existing.auth_json = auth_json;
        existing.updated_at = now;
        if usage.is_some() {
            existing.usage = usage;
        }
        existing.usage_error = usage_error;
        return Ok((existing.clone(), true));
    }

    let stored = StoredAccount {
        id: Uuid::new_v4().to_string(),
        label: resolved_label,
        email: extracted.email,
        account_id: extracted.account_id,
        plan_type: usage
            .as_ref()
            .and_then(|snapshot| snapshot.plan_type.clone())
            .or(extracted.plan_type),
        auth_json,
        added_at: now,
        updated_at: now,
        usage,
        usage_error,
    };
    store.accounts.push(stored.clone());
    Ok((stored, false))
}

pub fn import_store_into_store(
    target: &mut AccountsStore,
    source: &AccountsStore,
) -> Result<ImportStats> {
    let mut imported = 0usize;
    let mut updated = 0usize;

    for account in &source.accounts {
        let (stored, was_updated) = upsert_auth_account(
            target,
            account.auth_json.clone(),
            Some(account.label.clone()),
            account.usage.clone(),
            account.usage_error.clone(),
        )?;
        if was_updated {
            if stored.updated_at >= account.updated_at {
                updated += 1;
            }
        } else {
            imported += 1;
        }
    }

    Ok(ImportStats { imported, updated })
}

fn write_store_file(path: &Path, store: &AccountsStore) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("failed to resolve parent directory for {}", path.display())
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let serialized =
        serde_json::to_string_pretty(store).context("failed to serialize accounts store")?;
    write_file_atomically(path, serialized.as_bytes())
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("failed to resolve parent directory for {}", path.display())
    })?;
    let temp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("accounts.json"),
        Uuid::new_v4()
    ));

    let write_result = (|| -> Result<()> {
        let mut temp_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        temp_file
            .write_all(contents)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        temp_file
            .sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        drop(temp_file);
        set_private_permissions(&temp_path);
        fs::rename(&temp_path, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        set_private_permissions(path);
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result
}

fn normalize_custom_label(label: Option<String>) -> Option<String> {
    label.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn fallback_account_label(email: Option<&str>, account_id: &str) -> String {
    email
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("Codex {}", short_account(account_id)))
}

#[allow(dead_code)]
fn _backup_corrupted_store_file(path: &Path, raw: &str) -> Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("failed to resolve parent directory for {}", path.display())
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let backup_path = parent.join(format!("accounts.corrupt-{}.json", now_unix_seconds()));
    fs::write(&backup_path, raw)
        .with_context(|| format!("failed to write {}", backup_path.display()))?;
    set_private_permissions(&backup_path);
    Ok(backup_path)
}
