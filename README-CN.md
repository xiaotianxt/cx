# cx

[English README](README.md)

cx 是一个本地 Codex 入口：负责启动 Codex、处理 stdin pipe、管理多账号 slot，并按真实用量自动选择最合适的账号。

它把这几件事合成一个命令：

- `cx`：选择可用额度最高的 slot，进入 Codex。
- `cat file | cx "总结一下"`：把 stdin 包成上下文后进入 Codex TUI。
- `cx status`：并发查看所有 slot 的真实用量。
- `cx stats`：从本地 `state_5.sqlite` 汇总 Codex token 消耗。
- `cx add` / `cx login`：创建和登录独立 slot。

## 工作方式

每个 slot 是一个独立 `CODEX_HOME`：

```text
~/.codex/profile-manager/
  rotation.txt
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

`cx status` 里的 `5h` 列来自 `rate_limit.primary_window`，`weekly` 列来自
`rate_limit.secondary_window`。如果接口返回 reset 时间，summary 会显示下一次 refresh
还要多久。

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
```

stdin pipe：

```bash
cat README.md | cx "指出这个项目还缺什么"
git diff | cx "review 这个改动"
```

查看所有 slot：

```bash
cx status
cx status --json
```

只输出当前最优 slot：

```bash
cx select
```

查看本地 token 消耗：

```bash
cx stats
cx stats --by-slot
cx stats bus3
cx stats --json --no-price
cx stats --calibrate
```

`cx stats` 读取 Codex 本地维护的 `state_5.sqlite`，按 `threads.updated_at`
把 `threads.tokens_used` 汇总到 `1h`、`24h`、`today`、`week`、`month`、`year`。
人类可读输出会自动缩放 token 单位。价格估算是 best-effort：cx 会抓取并缓存 OpenAI
公开 API pricing 表，并优先使用 `cx stats --calibrate` 保存的 token mix。校准是显式触发的，
因为它需要扫描 rollout JSONL；普通 `cx stats` 只读取小的校准文件，或回退到内置 token mix。

新增一个 slot：

```bash
cx add bus6 --rotate
cx login bus6
```

从当前 `~/.codex` 复制登录态：

```bash
cx add work-a --rotate --from-current
```

新增外部 provider slot：

```bash
cx add deepseek --rotate \
  --set 'model_provider="deepseek"' \
  --set 'model="deepseek-v4-pro"' \
  --set 'model_providers.deepseek={ name = "DeepSeek", base_url = "https://api.deepseek.com", env_key = "DEEPSEEK_API_KEY", wire_api = "responses" }' \
  --env DEEPSEEK_API_KEY=sk-...
```

如果参数和 cx 子命令冲突，用 `--` 强制进入 Codex：

```bash
cx -- status
```

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

这些文件可能包含敏感信息，不要提交。

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
JSON 用 `serde_json`。并发使用标准库线程，不引入异步 runtime。

## 发版

维护者发版：

```bash
scripts/release.sh
```

脚本会运行测试、推送 tag、等待 GitHub Actions 产出 `darwin-arm64` release asset、更新
`xiaotianxt/homebrew-tap` 里的 `Formula/cx.rb`，并用 Homebrew 做一次安装验证。
