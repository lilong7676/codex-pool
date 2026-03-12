---
name: codex-pool
description: Manage multiple Codex CLI accounts with codex-pool, including checking 5h and 1week usage, switching to the best available account, re-authorizing expired accounts, and launching codex after switching.
version: 0.1.2
author: lilong7676
permissions:
  fileRead: true
  fileWrite: true
  network: true
  shell: true
permission_justification:
  fileRead: Read local Codex auth state, the installed codex-pool binary status, and local account store paths before acting.
  fileWrite: Install the codex-pool binary into the configured local bin directory and, after user confirmation, update ~/.codex/auth.json plus codex-pool's own state under ~/.codex-pool/.
  network: Download the pinned codex-pool release archive and checksum from this repository's GitHub Releases, refresh account usage against ChatGPT/OpenAI endpoints, and refresh login tokens when codex-pool needs to recover an expired session.
  shell: Run codex and codex-pool CLI commands needed for inspection, switching, installation, and re-authorization.
allowed_write_paths:
  - ${INSTALL_DIR:-$HOME/.local/bin}/codex-pool
  - ~/.codex/auth.json
  - ~/.codex-pool/accounts.json
  - ~/.codex-pool/config.toml
  - ~/.codex-pool/
allowed_network_hosts:
  - github.com
  - release-assets.githubusercontent.com
  - chatgpt.com
  - auth.openai.com
  - host from ~/.codex/config.toml chatgpt_base_url if the local Codex config overrides the default
  - host from auth.json token issuer (`iss` claim) if it differs from auth.openai.com
---

# Codex Pool

Use this skill when the user wants to inspect, switch, or repair Codex CLI accounts managed by `codex-pool`.

## Workflow

1. Check that the official `codex` CLI is installed before doing anything else.
2. Check whether `codex-pool` is already installed with `command -v codex-pool`, and if present compare its current version to the pinned `v0.1.2` skill version.
3. If `codex-pool` is missing or older than the pinned skill version, explain that the skill will download the pinned `v0.1.2` release archive and SHA256 file from this repository's GitHub Releases, verify the checksum, and install or upgrade the binary at `${INSTALL_DIR:-$HOME/.local/bin}/codex-pool`.
4. Only after explicit user confirmation, install or upgrade it with `CODEX_POOL_INSTALL_APPROVED=1 ./scripts/ensure-codex-pool.sh`.
5. Prefer non-mutating commands first:
   - `codex-pool doctor`
   - `codex-pool list --refresh`
   - `codex-pool watch`
   - `codex-pool refresh`
6. Only run mutating commands after the user clearly asks for them or explicitly confirms:
   - `codex-pool init`
   - `codex-pool add`
   - `codex-pool reauth <account-ref>`
   - `codex-pool use <account-ref>` or `codex-pool use --best`
   - `codex-pool run <account-ref> ...` or `codex-pool run --best ...`

## Safety Boundary

- `./scripts/ensure-codex-pool.sh` downloads from this repository's GitHub Releases (`github.com` with GitHub-owned release asset redirects) and installs or upgrades `codex-pool` in `${INSTALL_DIR:-$HOME/.local/bin}`.
- `codex-pool list --refresh`, `codex-pool watch`, and `codex-pool refresh` may call ChatGPT usage endpoints on `chatgpt.com`, or another host already configured in the local Codex config.
- Token refresh paths may call the OAuth issuer in the existing auth token, typically `auth.openai.com`.
- `codex-pool use`, `codex-pool run`, `codex-pool init`, `codex-pool add`, and `codex-pool reauth` may overwrite the live auth file at `~/.codex/auth.json`.
- `codex-pool init`, `codex-pool add`, `codex-pool reauth`, and other account-management commands may create or update `~/.codex-pool/accounts.json` and `~/.codex-pool/config.toml`.
- `codex-pool add`, `codex-pool init`, and `codex-pool reauth` may launch the browser via `codex login`.
- `codex-pool run ...` starts another process after switching auth; only use it when the user explicitly asks to launch `codex` or another program.
- `codex-pool update` downloads a GitHub Release archive plus checksum and overwrites the installed binary in place; only use it after explicit user confirmation.
- Do not install or upgrade `codex-pool`, switch accounts, start re-authorization, or launch a child process without explicit user confirmation.

## Recommended Sequence

- If `codex-pool` is missing or older than `v0.1.2`, stop and ask for install or upgrade confirmation before running the installer.
- Start with `codex-pool doctor` to confirm prerequisites.
- Use `codex-pool list --refresh` to inspect account status and usage.
- If the user wants the best account, prefer `codex-pool use --best` after confirmation.
- Only use `codex-pool run --best` when the user explicitly wants to launch `codex` or another program after switching.
- If an account is expired or requires login, use `codex-pool reauth <account-ref>` after confirmation.
- If the user explicitly wants to upgrade `codex-pool` outside the skill bootstrap flow, prefer `codex-pool update` or rerunning the published `install.sh`.

## References

- For command behavior and ranking rules, read `./references/commands.md`.
