# codex-pool command guide

Use these commands in this order unless the user asks for a different flow.

## Recommended sequence

1. `codex-pool doctor`
2. `codex-pool list --refresh`
3. `codex-pool use --best` or `codex-pool run --best`

## Non-mutating commands

- `codex-pool doctor`: checks whether `codex`, auth files, and the local setup look sane.
- `codex-pool list --refresh`: refreshes usage and shows account state, including whether an account is expired or needs reauth.
- `codex-pool watch`: repeatedly shows refreshed usage state.
- `codex-pool refresh [<account-ref>]`: updates usage data without switching accounts.

## Mutating commands

- `codex-pool init`: first-time setup flow. It may import the current auth, migrate legacy `codex-tools` accounts, and guide the user through adding accounts.
- `codex-pool add [--label ...]`: runs the official `codex login` flow, imports the new account, and restores the prior live auth afterward.
- `codex-pool reauth <account-ref>`: runs `codex login` again for one stored account. The newly logged-in `account_id` must match the target account or the operation fails.
- `codex-pool use <account-ref>`: writes the chosen account into `~/.codex/auth.json`.
- `codex-pool use --best`: writes the highest-ranked available account into `~/.codex/auth.json`.
- `codex-pool run ...`: switches auth first, then launches `codex`.

## Best-account ranking

`--best` uses a fixed ranking order:

1. Higher remaining `1week` ratio
2. Higher remaining `5h` ratio
3. Prefer the current live account
4. `label` as a stable tie-breaker

These states are excluded from `--best`:

- `expired`
- `workspace_removed`

## Account references

`<account-ref>` resolves in this priority order:

1. Exact match on the internal `id`
2. Exact match on `account_id`
3. Unique prefix match on `id` or `account_id`

If a prefix matches multiple accounts, the command fails and shows candidates.

## Platform support

- macOS: `aarch64`, `x86_64`
- Linux: `x86_64`

The skill installer script uses the same published GitHub Release assets as the project install script.
