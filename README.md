# codex-pool

[English](./README.md) | [简体中文](./README.zh-CN.md)

Thanks to [codex-tools](https://github.com/170-carry/codex-tools) for the proven multi-account management approach. The core account parsing, usage fetching, and switching flow in `codex-pool` is adapted and trimmed from that project.

`codex-pool` is a multi-account pool manager for Codex CLI. It is designed to help with:

- Managing multiple Codex accounts
- Viewing 5h / 1week usage for each account
- Switching to the best available account with one command
- Launching `codex` immediately after switching
- Re-authorizing expired accounts

Compared with the desktop `codex-tools`, `codex-pool` is:

- CLI-only, with no GUI, tray, proxy, or cloudflared features
- Using its own account store at `~/.codex-pool/accounts.json`
- Able to import accounts from the legacy `codex-tools` repository in one shot
- Still relying on `~/.codex/auth.json` as the live auth that actually takes effect

## Installation

After a release is published, install it with:

```bash
curl -fsSL https://github.com/lilong7676/codex-pool/releases/latest/download/install.sh | sh
```

By default, it installs to `~/.local/bin`. Override it with environment variables if needed:

```bash
INSTALL_DIR="$HOME/bin" VERSION="v0.1.1" curl -fsSL https://github.com/lilong7676/codex-pool/releases/latest/download/install.sh | sh
```

Prerequisites:

- The official `codex` CLI is already installed
- `codex login` can complete authorization successfully in your browser
- The first release supports macOS and Linux

## First-Time Setup

Run this after installation:

```bash
codex-pool init
```

`init` performs the following steps:

1. Check whether the `codex` CLI is available
2. Check whether the current `~/.codex/auth.json` exists and ask whether to import it
3. Detect a legacy `codex-tools` repository and ask whether to migrate it
4. Walk you through adding one or more accounts
5. Print a short summary of common commands at the end

Account adding does not implement OAuth independently. Instead, it reuses the official `codex login` flow:

- Back up the current `~/.codex/auth.json`
- Run `codex login`
- Wait for the new auth file to appear
- Import the new account into `codex-pool`
- Restore the previous live auth at the end

This means adding an account will not permanently replace the account you are currently using.

## Common Commands

List accounts:

```bash
codex-pool list
codex-pool list --refresh
codex-pool list --refresh --json
```

Watch usage:

```bash
codex-pool watch
codex-pool watch --interval 30
```

Add / remove accounts:

```bash
codex-pool add
codex-pool add --label "Work Pro"
codex-pool rm <account-ref>
```

Switch accounts:

```bash
codex-pool use <account-ref>
codex-pool use --best
```

Switch and launch `codex` immediately:

```bash
codex-pool run --best
codex-pool run --best -- exec "fix the failing tests"
codex-pool run <account-ref> -- app
```

Refresh usage:

```bash
codex-pool refresh
codex-pool refresh <account-ref>
```

Re-authorize an account:

```bash
codex-pool reauth <account-ref>
```

Run health checks:

```bash
codex-pool doctor
```

## Account Reference Rules

`<account-ref>` supports three forms with fixed priority:

1. Exact match on the internal `id`
2. Exact match on `account_id`
3. Unique prefix match on `id` or `account_id`

If a prefix matches multiple accounts, the command fails and prints the candidates.

## Best-Account Selection Rules

`--best` uses this fixed ranking order:

1. Compare remaining `1week` ratio first
2. Then compare remaining `5h` ratio
3. Prefer the current live account next
4. Finally use `label` as a stable tie-breaker

These states are excluded from `--best`:

- `expired`
- `workspace_removed`

## Re-Authorization

When an account's refresh token becomes invalid, `list --refresh` often shows:

- `expired`
- `reauth_required`

Then run:

```bash
codex-pool reauth <account-ref>
```

`reauth` runs the `codex login` flow again, but with one strict validation:

- The newly logged-in `account_id` must match the target account
- If your browser signs in to a different account, the operation fails and restores the previous live auth

## Migrate from codex-tools

If you previously used the desktop `codex-tools`, you can migrate its account store:

```bash
codex-pool import codex-tools
```

You can also provide the legacy repository path explicitly:

```bash
codex-pool import codex-tools --path /path/to/accounts.json
```

Default lookup paths:

- macOS: `~/Library/Application Support/com.carry.codex-tools/accounts.json`
- Linux: `~/.local/share/com.carry.codex-tools/accounts.json`

## Data Files

- `~/.codex-pool/accounts.json`: account store used by `codex-pool`
- `~/.codex-pool/config.toml`: `codex-pool` configuration
- `~/.codex/auth.json`: current live Codex auth; account switching writes this file directly

## Development

```bash
cargo test
cargo run -- --help
```

The release workflow builds these artifacts:

- `codex-pool-aarch64-apple-darwin.tar.gz`
- `codex-pool-x86_64-apple-darwin.tar.gz`
- `codex-pool-x86_64-unknown-linux-gnu.tar.gz`
- `install.sh`
