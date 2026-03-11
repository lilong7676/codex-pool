use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use dialoguer::Confirm;

use crate::auth;
use crate::context::AppContext;
use crate::models::AccountSummary;
use crate::models::AccountsStore;
use crate::models::ImportStats;
use crate::models::StoredAccount;
use crate::output::render_account_table;
use crate::ranking::pick_best_account;
use crate::store;
use crate::usage;
use crate::utils::short_account;

#[derive(Debug, Parser)]
#[command(
    name = "codex-pool",
    version,
    about = "Codex multi-account pool manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init,
    Add {
        #[arg(long)]
        label: Option<String>,
    },
    Rm {
        account_ref: String,
    },
    Reauth {
        account_ref: String,
    },
    Import {
        #[command(subcommand)]
        command: ImportCommands,
    },
    List {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    Watch {
        #[arg(long)]
        interval: Option<u64>,
    },
    Refresh {
        account_ref: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Use {
        account_ref: Option<String>,
        #[arg(long)]
        best: bool,
    },
    Run {
        account_ref: Option<String>,
        #[arg(long)]
        best: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        codex_args: Vec<String>,
    },
    Doctor,
}

#[derive(Debug, Subcommand)]
enum ImportCommands {
    CodexTools {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

pub async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let context = AppContext::discover()?;
    context.ensure_layout()?;

    match cli.command {
        Commands::Init => init_command(&context).await,
        Commands::Add { label } => {
            let account = add_account_via_login(&context, label, None).await?;
            println!(
                "added account {} ({})",
                account.label,
                short_account(&account.account_id)
            );
            Ok(())
        }
        Commands::Rm { account_ref } => remove_account_command(&context, &account_ref),
        Commands::Reauth { account_ref } => reauth_command(&context, &account_ref).await,
        Commands::Import { command } => match command {
            ImportCommands::CodexTools { path } => {
                import_codex_tools_command(&context, path.as_deref())
            }
        },
        Commands::List { refresh, json } => list_command(&context, refresh, json).await,
        Commands::Watch { interval } => watch_command(&context, interval).await,
        Commands::Refresh {
            account_ref,
            all,
            json,
        } => refresh_command(&context, account_ref.as_deref(), all, json).await,
        Commands::Use { account_ref, best } => {
            use_command(&context, account_ref.as_deref(), best).await
        }
        Commands::Run {
            account_ref,
            best,
            codex_args,
        } => run_command(&context, account_ref.as_deref(), best, &codex_args).await,
        Commands::Doctor => doctor_command(&context),
    }
}

async fn init_command(context: &AppContext) -> Result<()> {
    if context.codex_cli_path.is_none() {
        anyhow::bail!("codex executable not found; install Codex CLI first");
    }

    let mut changed = 0usize;
    if auth::read_current_codex_auth_optional(context)?.is_some()
        && Confirm::new()
            .with_prompt("Import the current ~/.codex/auth.json into codex-pool?")
            .default(true)
            .interact()?
    {
        let _ = import_current_auth(context, None).await?;
        changed += 1;
    }

    if let Some(legacy_path) = context.resolve_legacy_store_path(None) {
        let prompt = format!(
            "Import accounts from legacy Codex Tools store at {}?",
            legacy_path.display()
        );
        if Confirm::new()
            .with_prompt(prompt)
            .default(true)
            .interact()?
        {
            let stats = import_codex_tools(context, Some(&legacy_path))?;
            changed += stats.total_changed();
        }
    }

    loop {
        let should_add = Confirm::new()
            .with_prompt("Add another Codex account now?")
            .default(changed == 0)
            .interact()?;
        if !should_add {
            break;
        }
        let account = add_account_via_login(context, None, None).await?;
        println!(
            "added account {} ({})",
            account.label,
            short_account(&account.account_id)
        );
        changed += 1;
    }

    println!("\nNext commands:");
    println!("  codex-pool list");
    println!("  codex-pool run --best");
    println!("  codex-pool reauth <account-ref>");
    Ok(())
}

async fn list_command(context: &AppContext, refresh: bool, json: bool) -> Result<()> {
    let accounts = if refresh {
        refresh_accounts(context, None).await?
    } else {
        summaries_from_store(context, &store::load_store(context)?)
    };
    print_accounts(&accounts, json)?;
    Ok(())
}

async fn watch_command(context: &AppContext, interval: Option<u64>) -> Result<()> {
    let config = context.load_config()?;
    let interval = interval.unwrap_or(config.default_watch_interval_seconds);

    loop {
        let accounts = refresh_accounts(context, None).await?;
        print!("\x1B[2J\x1B[H");
        println!("{}", render_account_table(&accounts));
        io::Write::flush(&mut io::stdout())?;
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

async fn refresh_command(
    context: &AppContext,
    account_ref: Option<&str>,
    all: bool,
    json: bool,
) -> Result<()> {
    let target = if all { None } else { account_ref };
    let accounts = refresh_accounts(context, target).await?;
    print_accounts(&accounts, json)?;
    Ok(())
}

async fn use_command(context: &AppContext, account_ref: Option<&str>, best: bool) -> Result<()> {
    let account = select_target_account(context, account_ref, best).await?;
    auth::write_active_codex_auth(context, &account.auth_json)?;
    println!(
        "switched live auth to {} ({})",
        account.label,
        short_account(&account.account_id)
    );
    Ok(())
}

async fn run_command(
    context: &AppContext,
    account_ref: Option<&str>,
    best: bool,
    codex_args: &[String],
) -> Result<()> {
    let account = select_target_account(context, account_ref, best).await?;
    auth::write_active_codex_auth(context, &account.auth_json)?;

    let mut command = context.new_codex_command()?;
    if codex_args.is_empty() {
        let error = command.exec();
        return Err(anyhow::anyhow!("failed to exec codex: {error}"));
    }

    command.args(codex_args);
    let error = command.exec();
    Err(anyhow::anyhow!("failed to exec codex: {error}"))
}

fn doctor_command(context: &AppContext) -> Result<()> {
    let live = auth::read_current_auth_status(context)?;
    let store = store::load_store(context)?;
    println!(
        "codex_cli={}",
        context
            .codex_cli_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "missing".to_string())
    );
    println!("data_dir={}", context.paths.data_dir.display());
    println!("store_path={}", context.paths.store_path.display());
    println!("account_count={}", store.accounts.len());
    println!(
        "live_auth={}",
        if live.available {
            live.account_id.unwrap_or_else(|| "available".to_string())
        } else {
            "missing".to_string()
        }
    );
    println!(
        "legacy_store={}",
        context
            .resolve_legacy_store_path(None)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found".to_string())
    );
    Ok(())
}

fn remove_account_command(context: &AppContext, account_ref: &str) -> Result<()> {
    let mut store_value = store::load_store(context)?;
    let account = resolve_account(&store_value, account_ref)?.clone();
    store_value.accounts.retain(|item| item.id != account.id);
    store::save_store(context, &store_value)?;
    println!(
        "removed {} ({}) from codex-pool store",
        account.label,
        short_account(&account.account_id)
    );
    Ok(())
}

async fn reauth_command(context: &AppContext, account_ref: &str) -> Result<()> {
    let store_value = store::load_store(context)?;
    let account = resolve_account(&store_value, account_ref)?.clone();
    let refreshed = add_account_via_login(
        context,
        Some(account.label.clone()),
        Some(account.account_id.clone()),
    )
    .await?;
    println!(
        "reauthorized {} ({})",
        refreshed.label,
        short_account(&refreshed.account_id)
    );
    Ok(())
}

fn import_codex_tools_command(
    context: &AppContext,
    explicit_path: Option<&std::path::Path>,
) -> Result<()> {
    let stats = import_codex_tools(context, explicit_path)?;
    println!(
        "imported {} accounts, updated {} accounts",
        stats.imported, stats.updated
    );
    Ok(())
}

pub fn import_codex_tools(
    context: &AppContext,
    explicit_path: Option<&std::path::Path>,
) -> Result<ImportStats> {
    let path = context
        .resolve_legacy_store_path(explicit_path)
        .ok_or_else(|| anyhow::anyhow!("legacy Codex Tools store not found"))?;
    let source = store::load_store_from_path(&path)?;
    let mut target = store::load_store(context)?;
    let stats = store::import_store_into_store(&mut target, &source)?;
    store::save_store(context, &target)?;
    Ok(stats)
}

pub async fn import_current_auth(
    context: &AppContext,
    label: Option<String>,
) -> Result<StoredAccount> {
    let auth_json = auth::read_current_codex_auth(context)?;
    let usage = match auth::extract_auth(&auth_json) {
        Ok(extracted) => {
            usage::fetch_usage_snapshot(context, &extracted.access_token, &extracted.account_id)
                .await
                .ok()
        }
        Err(_) => None,
    };
    let mut store_value = store::load_store(context)?;
    let (account, _) = store::upsert_auth_account(&mut store_value, auth_json, label, usage, None)?;
    store::save_store(context, &store_value)?;
    Ok(account)
}

pub async fn add_account_via_login(
    context: &AppContext,
    label: Option<String>,
    expected_account_id: Option<String>,
) -> Result<StoredAccount> {
    let backup = auth::read_current_codex_auth_optional(context)?;
    let baseline = auth::read_current_auth_status(context)?.fingerprint;

    let login_result = async {
        let mut command = context.new_codex_command()?;
        let status = command
            .arg("login")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to start `codex login`")?;
        if !status.success() {
            anyhow::bail!("`codex login` exited with status {status}");
        }

        let auth_json = wait_for_new_auth(context, baseline.clone()).await?;
        let extracted = auth::extract_auth(&auth_json)?;
        if let Some(expected_account_id) = expected_account_id.as_deref() {
            if extracted.account_id != expected_account_id {
                anyhow::bail!(
                    "reauth returned account {} instead of expected {}",
                    extracted.account_id,
                    expected_account_id
                );
            }
        }

        let fetch_result =
            usage::fetch_usage_snapshot(context, &extracted.access_token, &extracted.account_id)
                .await;
        let (usage_snapshot, usage_error) = match fetch_result {
            Ok(snapshot) => (Some(snapshot), None),
            Err(error) => (
                None,
                Some(usage::normalize_usage_error_message(&error.to_string())),
            ),
        };

        let mut store_value = store::load_store(context)?;
        let (account, _) = store::upsert_auth_account(
            &mut store_value,
            auth_json,
            label,
            usage_snapshot,
            usage_error,
        )?;
        store::save_store(context, &store_value)?;
        Ok(account)
    }
    .await;

    let restore_result = auth::restore_active_codex_auth(context, backup);
    match (login_result, restore_result) {
        (Ok(account), Ok(())) => Ok(account),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore_error)) => {
            Err(anyhow::anyhow!("{error}; restore failed: {restore_error}"))
        }
    }
}

async fn wait_for_new_auth(
    context: &AppContext,
    baseline_fingerprint: Option<String>,
) -> Result<serde_json::Value> {
    let mut last_seen = None;
    for _ in 0..600 {
        let status = auth::read_current_auth_status(context)?;
        if status.available && status.fingerprint != baseline_fingerprint {
            if let Some(value) = auth::read_current_codex_auth_optional(context)? {
                return Ok(value);
            }
        }
        last_seen = status.fingerprint;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    anyhow::bail!(
        "timed out waiting for auth.json to change after login (last fingerprint: {:?})",
        last_seen
    );
}

pub async fn refresh_accounts(
    context: &AppContext,
    target_ref: Option<&str>,
) -> Result<Vec<AccountSummary>> {
    let mut store_value = store::load_store(context)?;
    let live_auth = auth::read_current_codex_auth_optional(context)?;
    let live_auth_account_id = live_auth
        .as_ref()
        .and_then(|value| auth::extract_auth(value).ok())
        .map(|auth| auth.account_id);
    let target_id = if let Some(reference) = target_ref {
        Some(resolve_account(&store_value, reference)?.account_id.clone())
    } else {
        None
    };

    for account in &mut store_value.accounts {
        if target_id
            .as_ref()
            .map(|target| target != &account.account_id)
            .unwrap_or(false)
        {
            continue;
        }

        let mut auth_json = if live_auth_account_id
            .as_ref()
            .map(|value| value == &account.account_id)
            .unwrap_or(false)
        {
            live_auth
                .clone()
                .unwrap_or_else(|| account.auth_json.clone())
        } else {
            account.auth_json.clone()
        };

        let mut refresh_error = None;
        let mut extracted = auth::extract_auth(&auth_json);
        let mut fetch_result = match &extracted {
            Ok(extracted) => {
                usage::fetch_usage_snapshot(context, &extracted.access_token, &extracted.account_id)
                    .await
            }
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
        };

        if usage::should_retry_with_token_refresh(&fetch_result) {
            match auth::refresh_chatgpt_auth_tokens(&auth_json).await {
                Ok(refreshed) => {
                    auth_json = refreshed;
                    extracted = auth::extract_auth(&auth_json);
                    fetch_result = match &extracted {
                        Ok(extracted) => {
                            usage::fetch_usage_snapshot(
                                context,
                                &extracted.access_token,
                                &extracted.account_id,
                            )
                            .await
                        }
                        Err(error) => Err(anyhow::anyhow!(error.to_string())),
                    };
                }
                Err(error) => {
                    refresh_error = Some(error.to_string());
                }
            }
        }

        account.updated_at = crate::utils::now_unix_seconds();
        account.auth_json = auth_json.clone();
        if let Ok(extracted) = &extracted {
            account.email = extracted.email.clone().or(account.email.clone());
            account.plan_type = extracted.plan_type.clone().or(account.plan_type.clone());
        }

        match fetch_result {
            Ok(mut snapshot) => {
                if snapshot.plan_type.is_none() {
                    snapshot.plan_type = account.plan_type.clone();
                }
                account.plan_type = snapshot.plan_type.clone().or(account.plan_type.clone());
                account.usage = Some(snapshot);
                account.usage_error = None;
            }
            Err(error) => {
                let normalized = if let Some(refresh_error) = refresh_error {
                    usage::normalize_usage_error_message(&format!(
                        "{} | token refresh failed: {}",
                        error, refresh_error
                    ))
                } else {
                    usage::normalize_usage_error_message(&error.to_string())
                };
                account.usage_error = Some(normalized);
            }
        }
    }

    store::save_store(context, &store_value)?;
    Ok(summaries_from_store(context, &store_value))
}

pub async fn select_target_account(
    context: &AppContext,
    account_ref: Option<&str>,
    best: bool,
) -> Result<StoredAccount> {
    match (best, account_ref) {
        (true, Some(_)) => anyhow::bail!("pass either --best or <account-ref>, not both"),
        (false, None) => anyhow::bail!("missing <account-ref> or --best"),
        _ => {}
    }

    let store_value = if best {
        let _ = refresh_accounts(context, None).await?;
        store::load_store(context)?
    } else {
        store::load_store(context)?
    };

    if best {
        let summaries = summaries_from_store(context, &store_value);
        let selected = pick_best_account(&summaries)
            .ok_or_else(|| anyhow::anyhow!("no switchable account found"))?;
        resolve_account(&store_value, &selected.account_id).cloned()
    } else {
        resolve_account(&store_value, account_ref.expect("validated above")).cloned()
    }
}

fn summaries_from_store(context: &AppContext, store_value: &AccountsStore) -> Vec<AccountSummary> {
    let current = auth::current_auth_account_id(context);
    let mut summaries = store_value
        .accounts
        .iter()
        .map(|account| account.to_summary(current.as_deref()))
        .collect::<Vec<_>>();
    summaries.sort_by(crate::ranking::compare_accounts_by_remaining);
    summaries
}

fn resolve_account<'a>(
    store_value: &'a AccountsStore,
    account_ref: &str,
) -> Result<&'a StoredAccount> {
    if let Some(exact) = store_value
        .accounts
        .iter()
        .find(|account| account.id == account_ref)
    {
        return Ok(exact);
    }
    if let Some(exact) = store_value
        .accounts
        .iter()
        .find(|account| account.account_id == account_ref)
    {
        return Ok(exact);
    }

    let matches = store_value
        .accounts
        .iter()
        .filter(|account| {
            account.id.starts_with(account_ref) || account.account_id.starts_with(account_ref)
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => anyhow::bail!("account reference `{account_ref}` not found"),
        [account] => Ok(account),
        many => {
            let mut message =
                format!("account reference `{account_ref}` is ambiguous. Candidates:");
            for candidate in many {
                message.push_str(&format!(
                    "\n- {} | {} | {}",
                    candidate.id, candidate.label, candidate.account_id
                ));
            }
            Err(anyhow::anyhow!(message))
        }
    }
}

fn print_accounts(accounts: &[AccountSummary], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(accounts).context("failed to serialize account list")?
        );
    } else {
        println!("{}", render_account_table(accounts));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::auth;
    use crate::context::AppContext;
    use crate::context::AppPaths;
    use crate::context::MockUsageResponse;
    use crate::models::AccountsStore;
    use crate::models::UsageSnapshot;
    use crate::models::UsageWindow;
    use crate::store;

    use super::add_account_via_login;
    use super::import_codex_tools;
    use super::refresh_accounts;
    use super::select_target_account;

    fn build_auth(account_id: &str, email: &str) -> serde_json::Value {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("access-{account_id}"),
                "refresh_token": format!("refresh-{account_id}"),
                "account_id": account_id,
                "id_token": format!("header.{}.sig", base64_url_payload(email, account_id))
            }
        })
    }

    fn base64_url_payload(email: &str, account_id: &str) -> String {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        URL_SAFE_NO_PAD.encode(format!(
            "{{\"email\":\"{email}\",\"https://api.openai.com/auth\":{{\"chatgpt_account_id\":\"{account_id}\",\"chatgpt_plan_type\":\"pro\"}}}}"
        ))
    }

    fn usage_snapshot(one_week_used: f64, five_hour_used: f64) -> UsageSnapshot {
        UsageSnapshot {
            fetched_at: 1,
            plan_type: Some("pro".to_string()),
            five_hour: Some(UsageWindow {
                used_percent: five_hour_used,
                window_seconds: 18000,
                reset_at: Some(10),
            }),
            one_week: Some(UsageWindow {
                used_percent: one_week_used,
                window_seconds: 604800,
                reset_at: Some(20),
            }),
            credits: None,
        }
    }

    fn test_context(temp: &TempDir) -> AppContext {
        let context = AppContext::with_paths(AppPaths::for_home(temp.path().to_path_buf()));
        context.ensure_layout().expect("layout should be created");
        context
    }

    fn write_fake_codex_script(
        temp: &TempDir,
        auth_path: &std::path::Path,
        auth_json: &serde_json::Value,
    ) -> std::path::PathBuf {
        let script_path = temp.path().join("codex");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"login\" ]; then\ncat <<'JSON' > \"{}\"\n{}\nJSON\nexit 0\nfi\nexit 0\n",
                auth_path.display(),
                serde_json::to_string_pretty(auth_json).expect("auth json should serialize")
            ),
        )
        .expect("script should be written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script_path)
                .expect("metadata should exist")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script_path, perms).expect("permissions should update");
        }
        script_path
    }

    #[tokio::test]
    async fn add_restores_original_live_auth() {
        let temp = TempDir::new().expect("tempdir should exist");
        let mut context = test_context(&temp);
        let original = build_auth("original-1", "orig@example.com");
        auth::write_active_codex_auth(&context, &original)
            .expect("original auth should be written");

        let new_auth = build_auth("new-1", "new@example.com");
        let script_path = write_fake_codex_script(&temp, &context.paths.live_auth_path, &new_auth);
        context.codex_cli_path = Some(script_path);
        context.test_hooks.mock_usage.insert(
            "new-1".to_string(),
            MockUsageResponse::Snapshot(usage_snapshot(10.0, 20.0)),
        );

        let account = add_account_via_login(&context, None, None)
            .await
            .expect("account should be added");
        assert_eq!(account.account_id, "new-1");

        let restored = auth::read_current_codex_auth(&context).expect("auth should restore");
        assert_eq!(
            auth::extract_auth(&restored)
                .expect("restored auth should parse")
                .account_id,
            "original-1"
        );
    }

    #[tokio::test]
    async fn reauth_fails_when_login_returns_different_account() {
        let temp = TempDir::new().expect("tempdir should exist");
        let mut context = test_context(&temp);
        let original = build_auth("current-1", "current@example.com");
        auth::write_active_codex_auth(&context, &original).expect("live auth should be written");

        let mut store_value = AccountsStore::default();
        let (stored, _) = store::upsert_auth_account(
            &mut store_value,
            build_auth("expected-1", "expected@example.com"),
            Some("Expected".to_string()),
            Some(usage_snapshot(30.0, 40.0)),
            None,
        )
        .expect("store upsert should work");
        store::save_store(&context, &store_value).expect("store should save");

        let script_path = write_fake_codex_script(
            &temp,
            &context.paths.live_auth_path,
            &build_auth("wrong-1", "wrong@example.com"),
        );
        context.codex_cli_path = Some(script_path);

        let error = add_account_via_login(
            &context,
            Some(stored.label.clone()),
            Some(stored.account_id.clone()),
        )
        .await
        .expect_err("reauth should reject mismatched account");
        assert!(error.to_string().contains("instead of expected"));

        let restored = auth::read_current_codex_auth(&context).expect("auth should restore");
        assert_eq!(
            auth::extract_auth(&restored)
                .expect("restored auth should parse")
                .account_id,
            "current-1"
        );
    }

    #[test]
    fn import_codex_tools_deduplicates_accounts() {
        let temp = TempDir::new().expect("tempdir should exist");
        let context = test_context(&temp);

        let mut target = AccountsStore::default();
        let _ = store::upsert_auth_account(
            &mut target,
            build_auth("acc-1", "one@example.com"),
            Some("One".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        store::save_store(&context, &target).expect("target store should save");

        let legacy_dir = temp.path().join("legacy");
        fs::create_dir_all(&legacy_dir).expect("legacy dir should exist");
        let legacy_path = legacy_dir.join("accounts.json");
        let mut legacy = AccountsStore::default();
        let _ = store::upsert_auth_account(
            &mut legacy,
            build_auth("acc-1", "one@example.com"),
            Some("One Updated".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        let _ = store::upsert_auth_account(
            &mut legacy,
            build_auth("acc-2", "two@example.com"),
            Some("Two".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        store::save_store_to_path(&legacy_path, &legacy).expect("legacy store should save");

        let stats = import_codex_tools(&context, Some(&legacy_path)).expect("import should work");
        assert_eq!(stats.imported, 1);
        assert_eq!(stats.updated, 1);
    }

    #[tokio::test]
    async fn use_best_and_run_best_share_same_selection_logic() {
        let temp = TempDir::new().expect("tempdir should exist");
        let mut context = test_context(&temp);
        let mut store_value = AccountsStore::default();
        let _ = store::upsert_auth_account(
            &mut store_value,
            build_auth("acc-1", "one@example.com"),
            Some("One".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        let _ = store::upsert_auth_account(
            &mut store_value,
            build_auth("acc-2", "two@example.com"),
            Some("Two".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        store::save_store(&context, &store_value).expect("store should save");
        context.test_hooks.mock_usage.insert(
            "acc-1".to_string(),
            MockUsageResponse::Snapshot(usage_snapshot(30.0, 40.0)),
        );
        context.test_hooks.mock_usage.insert(
            "acc-2".to_string(),
            MockUsageResponse::Snapshot(usage_snapshot(10.0, 20.0)),
        );

        let selected_for_use = select_target_account(&context, None, true)
            .await
            .expect("best account should be found");
        let selected_for_run = select_target_account(&context, None, true)
            .await
            .expect("best account should be found");
        assert_eq!(selected_for_use.account_id, selected_for_run.account_id);
        assert_eq!(selected_for_use.account_id, "acc-2");
    }

    #[tokio::test]
    async fn refresh_marks_healthy_and_expired_accounts() {
        let temp = TempDir::new().expect("tempdir should exist");
        let mut context = test_context(&temp);
        let mut store_value = AccountsStore::default();
        let _ = store::upsert_auth_account(
            &mut store_value,
            build_auth("healthy-1", "one@example.com"),
            Some("Healthy".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        let _ = store::upsert_auth_account(
            &mut store_value,
            build_auth("expired-1", "two@example.com"),
            Some("Expired".to_string()),
            None,
            None,
        )
        .expect("upsert should work");
        store::save_store(&context, &store_value).expect("store should save");

        context.test_hooks.mock_usage.insert(
            "healthy-1".to_string(),
            MockUsageResponse::Snapshot(usage_snapshot(10.0, 10.0)),
        );
        context.test_hooks.mock_usage.insert(
            "expired-1".to_string(),
            MockUsageResponse::Error("provided authentication token is expired".to_string()),
        );

        let summaries = refresh_accounts(&context, None)
            .await
            .expect("refresh should succeed");
        let healthy = summaries
            .iter()
            .find(|account| account.account_id == "healthy-1")
            .expect("healthy account should exist");
        let expired = summaries
            .iter()
            .find(|account| account.account_id == "expired-1")
            .expect("expired account should exist");
        assert_eq!(healthy.status.to_string(), "healthy");
        assert_eq!(expired.status.to_string(), "expired");
    }
}
