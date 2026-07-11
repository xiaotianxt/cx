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
- `cx prime`: plan and run tiny quota-window priming requests.
- `cx add` / `cx login` / `cx remove`: manage isolated Codex slots.
- `cx select`: print the best slot name for scripts.
- `cx desktop`: launch ChatGPT Desktop through a selected slot.
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
5. For automatic selection, prefer slots with at least 20% bottleneck capacity
   whose bottleneck window refreshes soonest, then prefer the higher score.
6. If every available slot is below that expected-session floor, pick the
   highest score instead of chasing an almost-empty refresh window.
7. If every live check fails due to a transient network error, fall back to the
   first transient slot so a temporary network failure does not block local work.

`credits.has_credits=false` is not treated as exhaustion. The real availability
signals are `rate_limit.allowed` and `rate_limit.limit_reached`.

`cx status` prints slots sorted by score descending by default, preserving
`rotation.txt` order for ties. Automatic launch selection is refresh-aware, so
the selected slot may be a lower-score slot whose bottleneck capacity refreshes
sooner. Use `cx status --sort rotation` to show `rotation.txt` or explicit
argument order instead. The `5h` column comes from `rate_limit.primary_window`
and the `weekly` column comes from
`rate_limit.secondary_window`. The summary includes the next refresh time when
the usage endpoint reports a reset timestamp. The status line also includes a
masked account label, using a masked email plus a short account-id suffix when
available, so accounts can be distinguished without printing full email
addresses.

Usage checks use a per-slot 30 second cache. Cache misses are refreshed through
an adaptive scheduler: `--jobs` caps local in-flight work, while a persisted
request pacer controls how quickly new usage requests start. The pacer starts at
125ms between live requests, recovers additively after successful refreshes, and
backs off multiplicatively when the endpoint returns `429`. `Retry-After` is
honored when present; otherwise cx writes a short cooldown and uses stale
per-slot cache entries for up to 10 minutes. `cx status`, `cx select`, and
automatic slot selection retry non-rate-limit transient refresh failures once by
default. Use `--jobs`, `--retries`, and `--timeout` on status/select/doctor
online checks when the local network needs different limits. Automatic launcher
selection also honors `CX_SLOT_USAGE_JOBS`, `CX_SLOT_USAGE_RETRIES`, and
`CX_SLOT_USAGE_TIMEOUT`.
Human `cx status` also shows a temporary progress line on stderr when both
stdout and stderr are interactive terminals. The line is cleared before the
final report and is disabled for `--json`, pipes, redirects, `--no-progress`,
or `CX_NO_PROGRESS`.

## Install

### Agent Skill

Install the optional Codex/agent skill for `cx` workflows:

```bash
npx -y github:xiaotianxt/skills cx
```

After the npm package is published:

```bash
npx -y @xiaotianxt/skills cx
```

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
cx stats --refresh-prices
cx stats --json
cx stats --json --refresh-prices
cx stats --calibrate
```

`cx stats` reads Codex's local `state_5.sqlite` and, when rollout JSONL files
are available, buckets timestamped `token_count` deltas by day for the selected
range. If a rollout is missing or cannot be parsed, it falls back to bucketing
`threads.tokens_used` by `threads.updated_at`. Rollout parsing is cached in
`stats-rollout-cache.sqlite` and invalidated by file fingerprint, so hot stats
runs avoid full JSONL rescans.

Human output auto-scales token units and includes best-effort price estimates by
default. It uses the cached OpenAI public API pricing table when available, exact
rollout token categories when available, and the saved token mix from
`cx stats --calibrate` as a fallback. Use `--no-price` for token-only human
output, or `--refresh-prices` to force a pricing table refresh. Calibration is
explicit because it scans rollout JSONL files.

`cx stats --json` emits schema v2 and stays token-only by default, omitting cost
fields entirely. `cx stats --json --refresh-prices` adds a `priceEstimate` object
plus per-period, per-slot, and per-model cost fields. cx-owned
`price-cache.json`, `stats-calibration.json`, and `stats-rollout-cache.sqlite`
live under the profile-manager directory. cx never rewrites Codex's upstream
`state_5.sqlite`.

Prime 5h quota windows before predictable heavy work:

```bash
cx prime plan
cx prime install
cx prime run --dry-run
cx prime status
cx prime uninstall
```

`cx prime plan` reads local `state_5.sqlite` files and the rollout token cache to
infer the heaviest local work hours, then shifts those hours earlier by the
configured lead time, 210 minutes by default. `cx prime install` writes a macOS
LaunchAgent with exact `StartCalendarInterval` entries for those local times.
The cx process does not stay resident; launchd starts `cx prime run` at each
planned time and coalesces missed calendar events when the Mac wakes from sleep.

`cx prime run` checks live usage first and only sends the tiny `codex exec
--ephemeral` request to eligible ChatGPT slots whose 5h window does not already
look active and whose weekly quota still has the configured safety margin.
By default, every eligible slot is primed in parallel, with a minimum 5% weekly
remaining and the prompt `Reply exactly: hi`. Use `--slot`, `--target`,
`--max-slots`, `--model`, or `--prompt` on `cx prime install` or `cx prime run`
when you need a narrower policy or explicit concurrency cap.

This is intentionally local and opportunistic. If the Mac is asleep, launchd
runs the missed check after wake; if it is powered off or cannot wake before the
work session, no local scheduler can start the remote quota window early.

Create and authenticate a slot:

```bash
cx add bus6 --rotate
cx login bus6
```

Launch ChatGPT Desktop with the same slot isolation:

```bash
cx desktop
cx desktop --slot bus6
cx desktop --target research
```

`cx desktop` starts the Desktop executable directly with `CODEX_HOME` set to the
selected slot home, so the Desktop process reads that slot's `auth.json` and
account state. The current working directory is opened with Desktop's
`--open-project` argument by default, which keeps the slot's Desktop project
list aligned with the shell project that launched it. `env.conf` values are
passed to the Desktop process; slot `overrides.conf` values are not forwarded to
Electron. By default, `cx desktop` refuses to launch while another ChatGPT Desktop
process is running, because a second launch may reuse the old Electron instance
and ignore the new slot environment. Quit ChatGPT Desktop before switching slots,
or pass `--allow-parallel` when you intentionally want to test parallel
instances. The default installation paths support both the current `ChatGPT.app`
name and the legacy `Codex.app` name. If ChatGPT Desktop is installed somewhere
else, use `--app-bin` or `CX_CODEX_DESKTOP_BIN`.

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

### Ollama Usage Cookies

For `model_provider="ollama"` API-key slots, `cx status` tries to read real
Ollama Cloud usage from browser cookies. By default it tries Helium first and
then Google Chrome's `Default` profile.

Set these in the slot `env.conf` to use a specific Chrome profile:

```sh
export CX_OLLAMA_COOKIE_SOURCE="chrome"
export CX_OLLAMA_CHROME_PROFILE="Profile 5"
```

For a fully explicit source, set `CX_OLLAMA_COOKIE_DB` and optionally
`CX_OLLAMA_KEYCHAIN_SERVICE` / `CX_OLLAMA_KEYCHAIN_ACCOUNT`.

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
