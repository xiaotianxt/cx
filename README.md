# cx

[Chinese README / 中文文档](README-CN.md)

cx is a fast local launcher for OpenAI Codex. It wraps the everyday Codex
workflow into one command: launch Codex, pass piped stdin into the TUI, manage
multiple local auth slots, and automatically pick the best available account by
live usage.

## What It Does

- `cx`: launch Codex through the slot with the most remaining usage.
- `cat file | cx "summarize this"`: pass stdin into Codex as prompt context.
- `cx status`: query all configured slots concurrently.
- `cx stats`: summarize local Codex token usage from `state_5.sqlite`.
- `cx add` / `cx login` / `cx remove`: manage isolated Codex slots.
- `cx select`: print the best slot name for scripts.
- `cx desktop`: launch Codex Desktop through a selected slot.
- `cx --target <name>`: launch through a named target-specific slot group and
  override set.
- `cx serve start` / `cx serve stop`: manage a foreground loopback Codex
  app-server through a selected slot.
- `cx service start`: run app-server and the Telegram adapter under one local
  background supervisor.
- `cx serve daemon` / `cx serve ping`: run and check the local cx control
  socket.
- `cx channel telegram run`: bridge allowed Telegram chats into cx sessions and
  leases.
- `cx protocol export`: export version-matched Codex App Server schemas and
  TypeScript bindings.
- `cx completions`: generate shell completion scripts with dynamic slot/model
  candidates.

cx is intended for people who use Codex heavily across multiple ChatGPT
accounts, workspaces, or model-provider configurations and want the launcher to
choose intelligently instead of rotating blindly.

## How It Works

Each slot is an isolated `CODEX_HOME`:

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

For ChatGPT-authenticated slots, cx calls the same usage endpoint used by Codex:

```text
GET https://chatgpt.com/backend-api/wham/usage
Authorization: Bearer <access_token>
ChatGPT-Account-ID: <account_id>
```

Selection logic:

1. Read slot names from `rotation.txt`.
2. Query every slot concurrently.
3. Skip slots where `allowed=false` or `limit_reached=true`.
4. Score each available slot as `min(5h remaining, weekly remaining)`.
5. Pick the highest score, preserving `rotation.txt` order for ties.
6. If every live check fails due to a transient network error, fall back to the
   first transient slot so a temporary network failure does not block local work.

`credits.has_credits=false` is not treated as exhaustion. The real availability
signals are `rate_limit.allowed` and `rate_limit.limit_reached`.

`cx status` prints slots sorted by score descending by default, preserving
`rotation.txt` order for ties. Use `cx status --sort rotation` to show
`rotation.txt` or explicit argument order instead. The `5h` column comes from
`rate_limit.primary_window` and the `weekly` column comes from
`rate_limit.secondary_window`. The summary includes the next refresh time when
the usage endpoint reports a reset timestamp. The status line also includes a
masked account label, using a masked email plus a short account-id suffix when
available, so accounts can be distinguished without printing full email
addresses.

## Install

### Homebrew

```bash
brew install xiaotianxt/tap/cx
```

Install the development build:

```bash
brew install --HEAD xiaotianxt/tap/cx
```

### From Source

Requires a Rust toolchain.

```bash
git clone https://github.com/xiaotianxt/cx.git
cd cx
make install-local
```

`make install-local` installs `cx` to `~/.local/bin/cx`. Make sure
`~/.local/bin` is in your `PATH`.

## Usage

Launch Codex through the best available slot:

```bash
cx
cx -m gpt-5.4
cx --slot bus1 -m gpt-5.4
cx --target research -m gpt-5.5
```

For a clean `cx` launch with no prompt, pipe, or forwarded Codex args, `cx`
checks the latest unarchived Codex session for the current working directory. If
that session is not currently open in another Codex TUI, `cx` runs
`codex resume <session-id>` automatically. If the latest session is active, `cx`
leaves the arguments unchanged so Codex starts a new session. Explicit Codex
subcommands such as `resume`, `exec`, `review`, `help`, and remote app-server
launches are never rewritten.

Pipe context into Codex:

```bash
cat README.md | cx "identify missing sections"
git diff | cx "review this change"
```

Inspect usage:

```bash
cx status
cx status --target research
cx status --sort rotation
cx status --json
cx select
cx select --target research
```

Inspect local token consumption:

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

`cx stats` reads Codex's local `state_5.sqlite` and, when rollout JSONL files
are available, buckets timestamped `token_count` deltas for `1h`, `24h`,
`today`, `week`, `month`, and `year`. If a rollout is missing or cannot be
parsed, it falls back to bucketing `threads.tokens_used` by `threads.updated_at`.
Human output auto-scales token units and stays local-only by default. Price
estimates are opt-in with `--price`; cx fetches and caches OpenAI's public API
pricing table, then uses exact rollout token categories when available or the
saved token mix from `cx stats --calibrate` as a fallback. Calibration is
explicit because it scans rollout JSONL files.

`cx stats --json` emits schema v2. Token-only JSON omits cost fields entirely;
`cx stats --price --json` adds a `priceEstimate` object plus per-period,
per-slot, and per-model cost fields. cx-owned `price-cache.json` and
`stats-calibration.json` are versioned with `schemaVersion` and are normalized
to the current file schema the first time cx reads them. cx never rewrites
Codex's upstream `state_5.sqlite`.

Create and authenticate a slot:

```bash
cx add bus6 --rotate
cx login bus6
```

Launch Codex Desktop with the same slot isolation:

```bash
cx desktop
cx desktop --slot bus6
cx desktop --target research
```

`cx desktop` starts the Desktop executable directly with `CODEX_HOME` set to the
selected slot home, so the Desktop app-server reads that slot's `auth.json` and
account state. `env.conf` values are passed to the Desktop process; slot
`overrides.conf` values are not forwarded to Electron. By default, `cx desktop`
refuses to launch while another Codex Desktop process is running, because a
second launch may reuse the old Electron instance and ignore the new slot
environment. Quit Codex Desktop before switching slots, or pass
`--allow-parallel` when you intentionally want to test parallel instances. If
Codex Desktop is installed somewhere else, use `--app-bin` or
`CX_CODEX_DESKTOP_BIN`.

Copy the current `~/.codex` auth state into a slot:

```bash
cx add work-a --rotate --from-current
```

Remove a slot from rotation without deleting its login files:

```bash
cx remove work-a
```

Delete the slot directory as well:

```bash
cx remove work-a --delete-files
```

Create a non-OpenAI provider slot:

```bash
cx add deepseek --rotate \
  --set 'model_provider="deepseek"' \
  --set 'model="deepseek-v4-pro"' \
  --set 'model_providers.deepseek={ name = "DeepSeek", base_url = "https://api.deepseek.com", env_key = "DEEPSEEK_API_KEY", wire_api = "responses" }' \
  --env DEEPSEEK_API_KEY=sk-...
```

Create a target-specific config:

```bash
cx target add research bus1 bus2 \
  --set 'model="gpt-5.5"' \
  --env CX_EXPERIMENT=research
cx target list
cx target show research
```

Targets live in `~/.codex/profile-manager/targets/<name>.toml`:

```toml
slots = ["bus1", "bus2"]
set = ['model="gpt-5.5"']

[env]
CX_EXPERIMENT = "research"
```

When a target has no `slots`, cx uses `rotation.txt`. Target `set` overrides
are passed after slot `overrides.conf`, and target env values are merged after
slot `env.conf`, so target policy wins over slot defaults. `cx target show`
prints env variable names rather than values and redacts sensitive-looking
override values.

## App Server Foundation

Start a foreground Codex app-server through cx's slot and target selection:

```bash
cx serve start
cx serve start --target research
cx serve start --slot bus1 --listen ws://127.0.0.1:17654
cx serve stop
cx serve stop --force --json
cx serve status
cx serve status --json
cx serve probe
cx serve probe --listen ws://127.0.0.1:17654 --json
cx serve threads --limit 20 --json
```

`cx serve start` only accepts loopback `ws://127.0.0.1:<port>` or
`ws://localhost:<port>` listeners. Port `0` is resolved to a concrete free
loopback port before Codex is spawned, then cx waits for app-server `/readyz`
and writes `serve/default.json` under the profile manager directory.

`cx serve stop` reads that state file, verifies the recorded process and ready
endpoint still match, then sends a graceful stop signal. If the state is stale,
it cleans only the cx state file. `--force` escalates to SIGKILL after
`--wait-timeout`.

`cx serve probe` connects to the saved or explicit loopback WebSocket URL,
sends the Codex App Server `initialize` handshake, then calls `thread/list` with
a one-row local state-db probe. This verifies the app-server protocol path
without starting a model turn.

`cx serve threads` uses the same app-server WebSocket adapter to list thread
summaries with `thread/list`. See [Codex App Server WebSocket](docs/app-server-ws.md)
for the complete command flow, raw message shape, and schema export workflow.

Run the local cx control daemon on a private Unix socket:

```bash
cx serve daemon
cx serve daemon --json
cx serve ping
cx serve ping --json
cx serve session create
cx serve session create --channel telegram:12345 --json
cx serve session list
cx serve session show <session-id>
cx serve lease acquire --session <session-id> --channel terminal --json
cx serve lease acquire --session <session-id> --channel telegram:12345 --steal
cx serve lease release --session <session-id> --token <lease-token>
cx serve event list --session <session-id> --json
cx serve shutdown
```

The daemon binds `<profile-manager>/serve/control.sock`, keeps the serve
directory private, and speaks a small versioned JSON line protocol. `cx serve
session create` creates a cx-owned session id, writes
`serve/sessions/<session-id>.json`, and appends a `session-created` record to
`serve/events.ndjson`. `cx serve lease acquire` records the channel that
currently owns the session, increments `leaseEpoch`, returns a lease token, and
appends a lease event. Acquiring a session with an active lease fails unless
`--steal` is explicit. `cx serve event list` reads the journal through the same
control socket. This is the future adapter entry point; it does not yet broker
turns or approvals.

This is only the local foundation for a future daemon/control-plane layer.
Telegram, WeChat, remote terminal, and other adapters must eventually talk to cx
rather than to the raw Codex app-server WebSocket, so cx can own slot rotation,
leases, approval routing, and audit state.

## Background Service

Run the app-server and Telegram adapter under one cx-owned supervisor:

```bash
cx service start --telegram-token-op-ref 'op://Private/Telegram/codex_xiaotian_bot'
cx service status
cx service logs
cx service stop
```

`cx service start` defaults to Telegram. It starts `cx serve start`, waits for
the app-server state, then starts `cx channel telegram run`. Use `--no-telegram`
when only the background app-server is wanted. Token handling stays out of
source control and logs: pass a normal token environment variable with
`--telegram-bot-token-env`, or pass a 1Password reference with
`--telegram-token-op-ref` and the supervisor will inject the token only into the
Telegram child process.

For login startup on macOS, install a launchd agent:

```bash
cx service install --telegram-token-op-ref 'op://Private/Telegram/codex_xiaotian_bot' --start
cx service uninstall
```

The service writes state and logs under `<profile-manager>/service/`. The
supervisor restarts child processes if they exit; `cx service stop` stops the
recorded children and then the supervisor.

## Telegram Channel

Run the Telegram adapter. On first use, when no chats are trusted yet, `run`
prints a one-time `/bind <secret>` message for onboarding:

```bash
export TELEGRAM_BOT_TOKEN=...
cx serve start
cx channel telegram run
cx channel telegram run --acquire-lease
cx service start --telegram-token-op-ref 'op://Private/Telegram/codex_xiaotian_bot'
cx channel telegram bind
cx channel telegram menu
cx channel telegram status --json
```

After at least one chat is trusted, `run` listens only to trusted local bindings
and any explicit `--allow-chat` values. The adapter uses Telegram long polling,
binds chats to cx sessions, records `channel-message-received` metadata events
without storing message text, and sends ordinary text into the running Codex
app-server. `run` and `bind` also synchronize the Telegram command menu. See
[Telegram Channel Adapter](docs/telegram-channel.md) for the command flow and
state file shape. Forum group topics are routed independently, and `/new`,
`/use`, `/sessions`, and `/close` provide named session routing inside a chat or
topic.

Export the Codex App Server protocol definitions for downstream clients:

```bash
cx protocol export --out /tmp/codex-app-protocol
cx protocol export --out /tmp/codex-app-protocol --json-schema
cx protocol export --out /tmp/codex-app-protocol --typescript
```

When no format flag is supplied, cx writes both `json-schema/` and
`typescript/` under the output directory by delegating to the selected Codex
binary's `app-server generate-json-schema` and `app-server generate-ts`
commands.

If a Codex prompt or argument conflicts with a cx management command, use `--`
to force launcher mode:

```bash
cx -- status
```

Install shell completions:

```bash
cx completions fish > ~/.config/fish/completions/cx.fish
cx completions zsh > ~/.zsh/completions/_cx
cx completions bash > ~/.local/share/bash-completion/completions/cx
```

The release formula installs these completions automatically for Homebrew users.
The generated scripts complete cx commands, launcher flags, local slot names, and
target names, and cached Codex model names. Dynamic candidates come from local
files only; tab completion does not call the live usage endpoint.

## Slot Files

`overrides.conf` contains one Codex `-c` override per line:

```toml
model_provider="deepseek"
model="deepseek-v4-pro"
```

`env.conf` contains simple shell-style environment variables:

```bash
export DEEPSEEK_API_KEY="sk-..."
```

These files may contain credentials. Do not commit slot directories,
target files with secret env values, `auth.json`, or real `env.conf` values.

## Environment Variables

- `CX_PROFILE_MANAGER_DIR`: profile-manager directory. Defaults to
  `~/.codex/profile-manager`.
- `CX_CODEX_BIN`: path to the real Codex binary.
- `CX_SLOT_USAGE_TIMEOUT`: per-slot usage request timeout in seconds.
- `CX_SLOT_DEBUG`: print slot-selection details before launch.
- `CX_DEBUG`: print stdin-wrapper diagnostics.
- `CX_BIN`: path to the cx binary, mostly useful for tests and non-standard
  installs.

## Development

```bash
make fmt
make check
cargo test
```

The project intentionally keeps dependencies small: `clap` for CLI parsing,
blocking `reqwest` for HTTP, `toml` for config parsing, `serde_json` for JSON,
and `base64` for local JWT claim decoding. Slot queries use standard-library
threads instead of an async runtime.

## Release

Maintainers can release with:

```bash
scripts/release.sh
```

The script runs tests, pushes a tag, waits for GitHub Actions to publish the
`darwin-arm64` release artifact, updates `Formula/cx.rb` in
`xiaotianxt/homebrew-tap`, and verifies the Homebrew install.
