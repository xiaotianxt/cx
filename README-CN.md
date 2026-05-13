# cx

[English README](README.md)

cx 是一个本地 Codex 入口：负责启动 Codex、处理 stdin pipe、管理多账号 slot，并按真实用量自动选择最合适的账号。

它把这几件事合成一个命令：

- `cx`：通过当前最合适的本地 slot 启动 Codex。
- `cat file | cx "总结一下"`：把 stdin 包成上下文后进入 Codex TUI。
- `cx status`：并发查看所有 slot 的真实用量。
- `cx stats`：从本地 `state_5.sqlite` 汇总 Codex token 消耗。
- `cx add` / `cx login` / `cx remove`：管理独立 slot。
- `cx desktop`：通过选中的 slot 启动 Codex Desktop。
- `cx --slot <name>` / `cx --target <name>`：强制使用指定 slot 或 target 启动。
- `cx completions`：生成 shell completion，支持动态补全 slot 和 model。

## 工作方式

每个 slot 是一个独立 `CODEX_HOME`：

```text
~/.codex/profile-manager/
  rotation.txt
  targets/
    research.toml
  slots/
    primary/
      home/
        auth.json
        config.toml -> ~/.codex/config.toml
      overrides.conf
      env.conf
    bus1/
      home/
      overrides.conf
      env.conf
```

ChatGPT 登录型 slot 会直接请求 Codex 当前使用的用量接口：

```text
GET https://chatgpt.com/backend-api/wham/usage
Authorization: Bearer <access_token>
ChatGPT-Account-ID: <account_id>
```

选择策略：

1. 并发查询 `rotation.txt` 里的所有 slot。
2. `allowed=false` 或 `limit_reached=true` 的 slot 视为耗尽并跳过。
3. 使用 `min(5h 剩余额度, weekly 剩余额度)` 作为 score。
4. 选择 score 最高的 slot；分数相同则保持 `rotation.txt` 顺序。
5. 如果所有在线检查都是临时网络错误，则回退到第一个临时失败的 slot，避免网络抖动直接阻塞工作。

`credits.has_credits=false` 不会被当成耗尽。这个字段只表示没有额外 credit，真正能不能用以
`rate_limit.allowed` 和 `rate_limit.limit_reached` 为准。

`cx status` 默认按 score 倒序显示，分数相同则保持 `rotation.txt` 顺序。使用
`cx status --sort rotation` 可以改为按 `rotation.txt` 或显式参数顺序显示。`5h` 列来自
`rate_limit.primary_window`，`weekly` 列来自 `rate_limit.secondary_window`。如果接口返回
reset 时间，summary 会显示下一次 refresh 还要多久。状态行也会展示脱敏账号标识：优先使用
脱敏 email，并在可用时补一个短 account id 后缀，这样能区分账号，但不会打印完整邮箱地址。

## 安装

### Homebrew

```bash
brew install xiaotianxt/tap/cx
```

安装开发版：

```bash
brew install --HEAD xiaotianxt/tap/cx
```

### 源码安装

需要 Rust 工具链。

```bash
git clone https://github.com/xiaotianxt/cx.git
cd cx
make install-local
```

`make install-local` 会安装 `~/.local/bin/cx`。请确认 `~/.local/bin` 在 `PATH` 中。

## 常用命令

直接进入 Codex：

```bash
cx
cx -m gpt-5.4
cx --slot bus1 -m gpt-5.4
cx --target research -m gpt-5.5
```

没有 `--slot` 或 `--target` 时，`cx` 会查询 rotation 中的 slot，并通过当前最合适的
slot 启动 Codex。显式 slot 或 target 启动使用同一套隔离机制，并跳过自动选择。

stdin pipe：

```bash
cat README.md | cx "指出这个项目还缺什么"
git diff | cx "review 这个改动"
```

查看所有 slot：

```bash
cx status
cx status --target research
cx status --sort rotation
cx status --json
```

只输出当前最优 slot：

```bash
cx select
cx select --target research
```

查看本地 token 消耗：

```bash
cx stats
cx stats --target research
cx stats --by-slot
cx stats bus3
cx stats --price
cx stats --price --refresh-prices
cx stats --json
cx stats --price --json
cx stats --calibrate
```

`cx stats` 读取 Codex 本地维护的 `state_5.sqlite`，按 `threads.updated_at`
把 `threads.tokens_used` 汇总到 `1h`、`24h`、`today`、`week`、`month`、`year`。
人类可读输出会自动缩放 token 单位，并且默认只读取本地数据。价格估算需要显式传
`--price`：cx 会抓取并缓存 OpenAI 公开 API pricing 表，并优先使用
`cx stats --calibrate` 保存的 token mix。校准是显式触发的，因为它需要扫描 rollout
JSONL；普通 `cx stats --price` 只读取小的校准文件，或回退到内置 token mix。

`cx stats --json` 输出 schema v2。默认 token-only JSON 完全不输出成本字段；
`cx stats --price --json` 会增加 `priceEstimate`，并在 period、slot、model 上输出成本字段。
cx 自己拥有的 `price-cache.json` 和 `stats-calibration.json` 都带 `schemaVersion`。
cx 不会改写 Codex 上游的 `state_5.sqlite`。

新增一个 slot：

```bash
cx add bus6 --rotate
cx login bus6
```

用同一套 slot 隔离启动 Codex Desktop：

```bash
cx desktop
cx desktop --slot bus6
cx desktop --target research
```

`cx desktop` 会直接启动 Desktop 可执行文件，并把 `CODEX_HOME` 设为选中的 slot home；
这样 Desktop 进程会读取这个 slot 的 `auth.json` 和账号状态。slot 的
`env.conf` 会注入到 Desktop 进程；`overrides.conf` 不会传给 Electron。默认情况下，
如果已经有 Codex Desktop 进程在运行，`cx desktop` 会拒绝继续启动，因为第二次启动可能复用
旧 Electron 实例，导致新的 slot 环境没有生效。切换 slot 前请先退出 Codex Desktop；如果你
明确要测试并行实例，可以传 `--allow-parallel`。如果 Desktop 安装在其他位置，可以用
`--app-bin` 或 `CX_CODEX_DESKTOP_BIN` 指定。

从当前 `~/.codex` 复制登录态：

```bash
cx add work-a --rotate --from-current
```

从轮换列表移除一个 slot，但保留登录文件：

```bash
cx remove work-a
```

同时删除 slot 目录：

```bash
cx remove work-a --delete-files
```

新增外部 provider slot：

```bash
cx add deepseek --rotate \
  --set 'model_provider="deepseek"' \
  --set 'model="deepseek-v4-pro"' \
  --set 'model_providers.deepseek={ name = "DeepSeek", base_url = "https://api.deepseek.com", env_key = "DEEPSEEK_API_KEY", wire_api = "responses" }' \
  --env DEEPSEEK_API_KEY=sk-...
```

新增 target-specific 配置：

```bash
cx target add research bus1 bus2 \
  --set 'model="gpt-5.5"' \
  --env CX_EXPERIMENT=research
cx target list
cx target show research
```

target 文件位于 `~/.codex/profile-manager/targets/<name>.toml`：

```toml
slots = ["bus1", "bus2"]
set = ['model="gpt-5.5"']

[env]
CX_EXPERIMENT = "research"
```

如果 target 没有配置 `slots`，cx 会使用 `rotation.txt`。target 的 `set` 会排在 slot
`overrides.conf` 后面，target env 会排在 slot `env.conf` 后面，因此 target 策略会覆盖
slot 默认值。`cx target show` 只显示环境变量名，不打印值，并会对看起来敏感的 override
值做脱敏。

如果参数和 cx 子命令冲突，用 `--` 强制进入 Codex：

```bash
cx -- status
```

安装 shell completion：

```bash
cx completions fish > ~/.config/fish/completions/cx.fish
cx completions zsh > ~/.zsh/completions/_cx
cx completions bash > ~/.local/share/bash-completion/completions/cx
```

release formula 会为 Homebrew 用户自动安装这些 completions。
生成的脚本会补全 cx 命令、launcher flags、本地 slot 名、target 名和本地缓存的 Codex model 名。
动态候选只读本地文件，不会调用在线用量接口。

## 配置文件

`overrides.conf` 每行是一条会传给 Codex 的 `-c` 配置：

```toml
model_provider="deepseek"
model="deepseek-v4-pro"
```

`env.conf` 支持简单的 shell 风格环境变量：

```bash
export DEEPSEEK_API_KEY="sk-..."
```

这些文件和 target 文件可能包含敏感信息，不要提交真实 secret。

## 环境变量

- `CX_PROFILE_MANAGER_DIR`：覆盖 profile-manager 目录，默认 `~/.codex/profile-manager`。
- `CX_CODEX_BIN`：指定真实 Codex 二进制。
- `CX_SLOT_USAGE_TIMEOUT`：每个 slot 的用量查询超时时间，单位秒。
- `CX_SLOT_DEBUG`：启动前打印 slot 选择详情。
- `CX_DEBUG`：打印 stdin pipe 包装调试信息。
- `CX_BIN`：指定 cx 自身路径，主要用于测试或非标准安装。

## 开发

```bash
make fmt
make check
cargo test
```

项目刻意保持小依赖面：CLI 用 `clap`，HTTP 用 `reqwest` blocking client，配置解析用 `toml`，
JSON 用 `serde_json`，本地 JWT claim 解码用 `base64`。并发使用标准库线程，不引入异步 runtime。

## 发版

维护者发版：

```bash
scripts/release.sh
```

脚本会运行测试、推送 tag、等待 GitHub Actions 产出 `darwin-arm64` release asset、更新
`xiaotianxt/homebrew-tap` 里的 `Formula/cx.rb`，并用 Homebrew 做一次安装验证。
