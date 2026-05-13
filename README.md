# cx

[Chinese README / 中文文档](README-CN.md)

cx is a fast local launcher for OpenAI Codex. It wraps the everyday Codex
workflow into one command: launch Codex, pass piped stdin into the TUI, manage
multiple local auth slots, and automatically pick the best available account by
live usage.

## What It Does

- `cx`: launch Codex through the best available local slot.
- `cat file | cx "summarize this"`: pass stdin into Codex as prompt context.
- `cx status`: query all configured slots concurrently.
- `cx stats`: summarize local Codex token usage from `state_5.sqlite`.
- `cx add` / `cx login` / `cx remove`: manage isolated Codex slots.
- `cx select`: print the best slot name for scripts.
- `cx desktop`: launch Codex Desktop through a selected slot.
- `cx --slot <name>` / `cx --target <name>`: force a launch through that slot
  or target.
- `cx completions`: generate launcher shell completions with dynamic slot names.

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

Usage checks are live, not cached. To avoid burst failures with many accounts,
`cx status`, `cx select`, and automatic slot selection query at most 4 slots at
once and retry transient refresh failures once by default. Use `--jobs`,
`--retries`, and `--timeout` on status/select/doctor online checks when the
local network needs different limits. Automatic launcher selection also honors
`CX_SLOT_USAGE_JOBS`, `CX_SLOT_USAGE_RETRIES`, and `CX_SLOT_USAGE_TIMEOUT`.
Human `cx status` also shows a temporary progress line on stderr when both
stdout and stderr are interactive terminals. The line is cleared before the
final report and is disabled for `--json`, pipes, redirects, `--no-progress`,
or `CX_NO_PROGRESS`.

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

Without `--slot` or `--target`, `cx` queries rotation slots and launches Codex
through the best available one. Explicit slot or target launches use the same
slot isolation and skip automatic selection.

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
`stats-calibration.json` include `schemaVersion`. cx never rewrites Codex's
upstream `state_5.sqlite`.

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
selected slot home, so the Desktop process reads that slot's `auth.json` and
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
The generated scripts complete cx launcher flags before `--` and local slot
names for `--slot`/`-s`. Arguments after `--` belong to Codex and are not
completed by cx.

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
- `CX_DISABLE_STARTUP_REPAIR`: skip startup profile repair. Intended only for
  debugging a broken local profile.

## Upgrades

cx runs one-time startup repairs for profile-manager layouts created by already
published versions. The current repair covers stats cache schemas, slot sqlite
placement, and removed runtime state from tags through `v0.4.1`.

See [Startup Upgrade Repairs](docs/upgrades.md) for the public version matrix
and repair behavior.

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
