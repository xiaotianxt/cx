# cx

[English README](README.md)

cx 是一个本地 Codex 入口：负责启动 Codex、处理 stdin pipe、管理多账号 slot，并按真实用量自动选择最合适的账号。

它把这几件事合成一个命令：

- `cx`：通过当前最合适的本地 slot 启动 Codex。
- `cat file | cx "总结一下"`：把 stdin 包成上下文后进入 Codex TUI。
- `cx status`：并发查看所有 slot 的真实用量。
- `cx stats`：从本地 `state_5.sqlite` 汇总 Codex token 消耗。
- `cx prime`：按本地使用规律提前触发极短请求，启动 5h 额度窗口。
- `cx add` / `cx login` / `cx remove`：管理独立 slot。
- `cx desktop`：通过选中的 slot 启动 ChatGPT Desktop。
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

当一个 slot 同时存在 `auth.json` 和由 Keychain 支持的 `keychain.conf` 时，
cx 按以下顺序使用第一个可用的认证来源：

1. `auth.json`
2. Keychain PAT（仅作为 fallback）

启动时的判定完全在本地完成，并与 Codex 当前的 `AuthDotJson` 结构保持一致。
ChatGPT 认证必须包含可解码的 ID token、access/refresh token 字段和合法的
`last_refresh`；access token 已过期时还必须有非空 refresh token。API key、
auth.json 内 PAT、Agent Identity 和 Bedrock 模式则必须带各自对应的凭据字段。
`auth.json` 缺失、JSON 损坏、认证模式与凭据不匹配，或凭据在本地已确定不可用
时，cx 才启用 PAT fallback。cx 不会在启动热路径中增加在线认证探测；OAuth
刷新仍由 Codex 负责。`auth.json` 胜出时，cx 会屏蔽从父进程继承的认证环境
变量；slot 或 target 显式配置的竞争认证变量则会触发清晰报错。

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
4. 自动选择时，优先选瓶颈剩余额度至少 20%、且瓶颈窗口最早 refresh 的 slot；
   refresh 时间相同再选 score 更高的。
5. 如果所有可用 slot 都低于这个预期单次会话门槛，则回退到 score 最高的 slot，
   避免为了快重置而选中几乎空掉的账号。
6. 如果所有在线检查都是临时网络错误，则回退到第一个临时失败的 slot，避免网络抖动直接阻塞工作。

`credits.has_credits=false` 不会被当成耗尽。这个字段只表示没有额外 credit，真正能不能用以
`rate_limit.allowed` 和 `rate_limit.limit_reached` 为准。

`cx status` 默认按 score 倒序显示，分数相同则保持 `rotation.txt` 顺序。自动启动选择会考虑
瓶颈窗口的 refresh 时间，因此被选中的 slot 可能不是当前 score 最高的 slot。使用
`cx status --sort rotation` 可以改为按 `rotation.txt` 或显式参数顺序显示。`5h` 列来自
`rate_limit.primary_window`，`weekly` 列来自 `rate_limit.secondary_window`。如果接口返回
reset 时间，summary 会显示下一次 refresh 还要多久。状态行也会展示脱敏账号标识：优先使用
脱敏 email，并在可用时补一个短 account id 后缀，这样能区分账号，但不会打印完整邮箱地址。

用量检查使用 per-slot 30 秒缓存。cache miss 会进入自适应调度器：`--jobs` 只限制本地同时
进行的刷新数，持久化 request pacer 负责控制新请求启动速度。pacer 默认 live request 间隔
125ms；成功刷新后加性恢复，接口返回 `429` 时乘性降速。接口带 `Retry-After` 时直接遵守；
没有时写入短 cooldown，并在最多 10 分钟内用 stale per-slot cache 兜底。`cx status`、
`cx select` 和自动 slot 选择默认会对非 rate-limit 的临时刷新失败重试 1 次。status/select/
doctor online 检查可以用 `--jobs`、`--retries` 和 `--timeout` 调整本地网络策略；自动启动
选择也支持 `CX_SLOT_USAGE_JOBS`、`CX_SLOT_USAGE_RETRIES` 和 `CX_SLOT_USAGE_TIMEOUT`。
human `cx status` 在 stdout 和 stderr 都是交互式终端时，会在 stderr 显示一行临时进度。
最终报告打印前会清掉这行；`--json`、pipe、redirect、`--no-progress` 或 `CX_NO_PROGRESS`
都会禁用它。

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
cx stats --refresh-prices
cx stats --json
cx stats --json --refresh-prices
cx stats --calibrate
```

`cx stats` 读取共享的 `~/.codex/sqlite/state_5.sqlite`；如果 rollout JSONL 存在，就按选中
range 聚合 timestamped `token_count` delta。rollout 缺失或无法解析时，才回退到
`threads.updated_at` 上的 `threads.tokens_used`。rollout 解析结果会缓存在
`stats-rollout-cache.sqlite`，用文件 fingerprint 失效，热路径不会反复全量扫描 JSONL。

人类可读输出会自动缩放 token 单位，并默认展示 best-effort 价格估算。它会优先使用
已缓存的 OpenAI 公开 API pricing 表、可用时的精确 rollout token 分类，以及
`cx stats --calibrate` 保存的 token mix。需要 token-only 输出时用 `--no-price`；需要强制刷新
pricing 表时用 `--refresh-prices`。校准是显式触发的，因为它需要扫描 rollout JSONL。

`cx stats --json` 输出 schema v2，并默认保持 token-only，完全不输出成本字段；
`cx stats --json --refresh-prices` 会增加 `priceEstimate`，并在 period、slot、model 上输出
成本字段。cx 自己拥有的 `price-cache.json`、`stats-calibration.json` 和
`stats-rollout-cache.sqlite` 都位于 profile-manager 目录下。stats 只读 Codex 状态数据库。

在可预测的高强度工作前提前启动 5h 额度窗口：

```bash
cx prime plan
cx prime install
cx prime run --dry-run
cx prime status
cx prime uninstall
```

`cx prime plan` 会读取共享的 `state_5.sqlite` 和 rollout token cache，推断最近
高负载工作最常出现的本地小时，然后按 lead time 向前平移，默认提前 210 分钟。
`cx prime install` 会写入 macOS LaunchAgent，使用精确的 `StartCalendarInterval` 时间点。
cx 进程不会常驻后台；到点时由 launchd 拉起 `cx prime run`，Mac 睡眠期间错过的 calendar
事件会在唤醒后合并执行一次。

`cx prime run` 会先查实时用量，只对符合条件的 ChatGPT slot 发送极短的 `codex exec
--ephemeral` 请求：5h 窗口看起来尚未启动，并且 weekly 额度仍高于安全线。默认策略较保守：
默认会并发 prime 所有符合条件的 slot，weekly 剩余至少 5%，prompt 为 `Reply exactly: hi`。
需要收窄策略或显式限制并发时，可在 `cx prime install` 或 `cx prime run` 上使用 `--slot`、
`--target`、`--max-slots`、`--model` 或 `--prompt`。

这套机制是本地、机会式的。Mac 睡眠时，launchd 会在唤醒后执行错过的检查；如果机器关机，
或在正式开工前根本无法唤醒，本地 scheduler 没有机会提前启动远端额度窗口。

新增一个 slot：

```bash
cx add bus6 --rotate
cx login bus6
```

用同一套 slot 隔离启动 ChatGPT Desktop：

```bash
cx desktop
cx desktop --slot bus6
cx desktop --target research
```

`cx desktop` 会直接启动 Desktop 可执行文件，并把 `CODEX_HOME` 设为选中的 slot home；
这样 Desktop 进程会读取这个 slot 的 `auth.json` 和账号状态。默认会通过 Desktop 的
`--open-project` 参数打开当前工作目录，让这个 slot 的 Desktop 项目列表和启动它的 shell
项目保持一致。所有 slot 都使用同一个 `~/.codex/sqlite`，因此 Desktop 和 CLI 读取的是同一份
完整本地索引。CX 还会把 Desktop 默认的当前-provider 列表筛选改为所有 provider。具体
transport 与认证配置在启动时统一映射到稳定的运行身份 `cx`，因此新会话不再记录
slot 专属的 provider 名称。

slot 的 `env.conf` 会注入到 Desktop 进程；`overrides.conf` 不会传给 Electron。默认情况下，
如果已经有 ChatGPT Desktop 进程在运行，`cx desktop` 会拒绝继续启动，
因为第二次启动可能复用旧 Electron 实例，导致新的 slot 环境没有生效。切换 slot 前请先退出
ChatGPT Desktop；如果你明确要测试并行实例，可以传 `--allow-parallel`。默认安装路径同时兼容
当前的 `ChatGPT.app` 和旧版 `Codex.app`。如果 Desktop 安装在其他位置，可以用 `--app-bin`
或 `CX_CODEX_DESKTOP_BIN` 指定。

从曾经使用 per-slot SQLite 的版本升级时，CX 会把各 slot 遗留数据库合并到
`~/.codex/sqlite`。重复 thread 采用更新时间较新的索引行，并补齐缺失的关联行、memory 和 goal。
合并成功后，CX 会删除已经迁移的 per-slot 数据库及其 SQLite sidecar，也会删除不属于会话历史的
旧 per-slot 诊断日志。运行迁移前，请先退出由旧版 CX 启动的 Codex 和 Desktop 进程，
避免它们继续写入已经退役的 per-slot 数据库：

```bash
cx merge-sqlite --dry-run
cx merge-sqlite
```

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

### Ollama 用量 Cookie

`model_provider="ollama"` 的 API-key slot 在 `cx status` 中会尝试从浏览器
cookie 读取真实 Ollama Cloud 用量。默认顺序是 Helium，然后 Google Chrome 的
`Default` profile。

如果要指定 Chrome profile，在该 slot 的 `env.conf` 中设置：

```sh
export CX_OLLAMA_COOKIE_SOURCE="chrome"
export CX_OLLAMA_CHROME_PROFILE="Profile 5"
```

如果要完全指定来源，可以设置 `CX_OLLAMA_COOKIE_DB`，并按需设置
`CX_OLLAMA_KEYCHAIN_SERVICE` / `CX_OLLAMA_KEYCHAIN_ACCOUNT`。
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
- `CX_DISABLE_STARTUP_REPAIR`：跳过启动时 profile 修复，仅用于调试损坏的本地 profile。

## 升级

cx 会针对已经公开发布过的版本创建的一次性 profile-manager 布局问题做启动修复。
当前修复覆盖 stats cache schema、per-slot 到共享 SQLite 的迁移，以及 `v0.4.1` 之前移除运行时留下的状态。

公开版本范围和具体修复行为见 [Startup Upgrade Repairs](docs/upgrades.md)。

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
