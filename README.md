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
```

Pipe context into Codex:

```bash
cat README.md | cx "identify missing sections"
git diff | cx "review this change"
```

Inspect usage:

```bash
cx status
cx status --sort rotation
cx status --json
cx select
```

Inspect local token consumption:

```bash
cx stats
cx stats --by-slot
cx stats bus3
cx stats --price
cx stats --price --refresh-prices
cx stats --json
cx stats --price --json
cx stats --calibrate
```

`cx stats` reads Codex's local `state_5.sqlite` and buckets
`threads.tokens_used` by `threads.updated_at` for `1h`, `24h`, `today`, `week`,
`month`, and `year`. Human output auto-scales token units and stays local-only
by default. Price estimates are opt-in with `--price`; cx fetches and caches
OpenAI's public API pricing table, then uses the saved token mix from
`cx stats --calibrate` when available. Calibration is explicit because it scans
rollout JSONL files; normal `cx stats --price` only reads the small saved
calibration file or falls back to a built-in token mix.

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
cached Codex model names. Dynamic candidates come from local files only; tab
completion does not call the live usage endpoint.

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
`auth.json`, or real `env.conf` values.

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
