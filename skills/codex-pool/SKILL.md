---
name: codex-pool
description: Manage multiple Codex CLI accounts with codex-pool, including checking 5h and 1week usage, switching to the best available account, re-authorizing expired accounts, and launching codex after switching.
---

# Codex Pool

Use this skill when the user wants to inspect, switch, or repair Codex CLI accounts managed by `codex-pool`.

## Workflow

1. Check that the official `codex` CLI is installed before doing anything else.
2. Ensure `codex-pool` is available by running `./scripts/ensure-codex-pool.sh`.
3. Prefer non-mutating commands first:
   - `codex-pool doctor`
   - `codex-pool list --refresh`
   - `codex-pool watch`
   - `codex-pool refresh`
4. Only run mutating commands after the user clearly asks for them or explicitly confirms:
   - `codex-pool init`
   - `codex-pool add`
   - `codex-pool reauth <account-ref>`
   - `codex-pool use <account-ref>` or `codex-pool use --best`
   - `codex-pool run <account-ref> ...` or `codex-pool run --best ...`

## Safety Boundary

- `codex-pool use`, `codex-pool run`, `codex-pool init`, `codex-pool add`, and `codex-pool reauth` may overwrite the live auth file at `~/.codex/auth.json`.
- `codex-pool add`, `codex-pool init`, and `codex-pool reauth` may launch the browser via `codex login`.
- Do not switch accounts or start re-authorization without explicit user confirmation.

## Recommended Sequence

- Start with `codex-pool doctor` to confirm prerequisites.
- Use `codex-pool list --refresh` to inspect account status and usage.
- If the user wants the best account, prefer `codex-pool use --best` or `codex-pool run --best` after confirmation.
- If an account is expired or requires login, use `codex-pool reauth <account-ref>` after confirmation.

## References

- For command behavior and ranking rules, read `./references/commands.md`.
