# codex-pool

`codex-pool` 是一个面向 Codex CLI 的多账号池管理工具，解决这几件事：

- 管理多个 Codex 账号
- 查看每个账号的 5h / 1week 用量
- 一键切换到最佳可用账号
- 直接切换后启动 `codex`
- 对过期账号执行重新授权

它和桌面版 `codex-tools` 的关系是：

- `codex-pool` 只做 CLI，不做 GUI、tray、proxy、cloudflared
- `codex-pool` 独立使用 `~/.codex-pool/accounts.json`
- `codex-pool` 可以从旧版 `codex-tools` 仓库一次性导入账号
- 真正生效的 live auth 仍然是 `~/.codex/auth.json`

## 安装

发布后可直接执行：

```bash
curl -fsSL https://github.com/lilong7676/codex-pool/releases/latest/download/install.sh | sh
```

默认安装到 `~/.local/bin`。可通过环境变量覆盖：

```bash
INSTALL_DIR="$HOME/bin" VERSION="v0.1.0" curl -fsSL https://github.com/lilong7676/codex-pool/releases/latest/download/install.sh | sh
```

前置要求：

- 已安装官方 `codex` CLI
- `codex login` 可正常在浏览器中完成授权
- 首版支持 macOS 和 Linux

## 首次引导

安装后运行：

```bash
codex-pool init
```

`init` 会依次做这些事：

1. 检查 `codex` CLI 是否可用
2. 检查当前 `~/.codex/auth.json` 是否存在，并询问是否导入
3. 探测旧版 `codex-tools` 仓库，并询问是否迁移
4. 循环引导你添加一个或多个账号
5. 最后给出常用命令提示

添加账号的方式不是自己实现 OAuth，而是借用官方 `codex login`：

- 先备份当前 `~/.codex/auth.json`
- 运行 `codex login`
- 等待新的授权文件出现
- 导入新账号进 `codex-pool`
- 最后恢复原来的 live auth

这意味着你在添加账号时，不会把当前正在使用的账号永久改掉。

## 常用命令

查看账号列表：

```bash
codex-pool list
codex-pool list --refresh
codex-pool list --refresh --json
```

监控用量：

```bash
codex-pool watch
codex-pool watch --interval 30
```

添加 / 删除账号：

```bash
codex-pool add
codex-pool add --label "Work Pro"
codex-pool rm <account-ref>
```

切换账号：

```bash
codex-pool use <account-ref>
codex-pool use --best
```

切换后直接启动 `codex`：

```bash
codex-pool run --best
codex-pool run --best -- exec "fix the failing tests"
codex-pool run <account-ref> -- app
```

刷新用量：

```bash
codex-pool refresh
codex-pool refresh <account-ref>
```

重新授权：

```bash
codex-pool reauth <account-ref>
```

健康检查：

```bash
codex-pool doctor
```

## 账号引用规则

`<account-ref>` 支持三种形式，优先级固定：

1. 精确匹配内部 `id`
2. 精确匹配 `account_id`
3. 唯一前缀匹配 `id` 或 `account_id`

如果前缀匹配到多个账号，命令会报错并列出候选。

## 最佳账号切换规则

`--best` 的排序口径固定为：

1. 优先比较 `1week` 剩余比例
2. 再比较 `5h` 剩余比例
3. 再偏向当前 live account
4. 最后按 label 稳定排序

这些状态不会参与 `--best`：

- `expired`
- `workspace_removed`

## 重新授权

当某个账号的 refresh token 失效后，`list --refresh` 往往会显示：

- `expired`
- 或 `reauth_required`

此时执行：

```bash
codex-pool reauth <account-ref>
```

`reauth` 会重新走一次 `codex login`，但有一个硬校验：

- 新登录出来的 `account_id` 必须和目标账号一致
- 如果你浏览器里登录成了另一个账号，操作会失败并恢复原来的 live auth

## 从 codex-tools 迁移

如果你之前用过桌面版 `codex-tools`，可以迁移账号仓库：

```bash
codex-pool import codex-tools
```

也可以指定旧仓库路径：

```bash
codex-pool import codex-tools --path /path/to/accounts.json
```

默认探测路径：

- macOS: `~/Library/Application Support/com.carry.codex-tools/accounts.json`
- Linux: `~/.local/share/com.carry.codex-tools/accounts.json`

## 数据文件

- `~/.codex-pool/accounts.json`: `codex-pool` 的账号仓库
- `~/.codex-pool/config.toml`: `codex-pool` 配置
- `~/.codex/auth.json`: 当前 live Codex auth，切换账号时直接写这个文件

## 开发

```bash
cargo test
cargo run -- --help
```

发布 workflow 会构建这些产物：

- `codex-pool-aarch64-apple-darwin.tar.gz`
- `codex-pool-x86_64-apple-darwin.tar.gz`
- `codex-pool-x86_64-unknown-linux-gnu.tar.gz`
- `install.sh`
