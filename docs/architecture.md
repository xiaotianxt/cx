# Architecture

cx keeps the existing local Codex profile-manager layout and replaces shell
wrappers with a single Rust binary.

## Modules

- `cli`: typed clap commands for management operations.
- `cx`: default launcher and stdin pipe wrapper.
- `run`: argument parsing and `exec` into the real Codex binary.
- `slot`: slot layout, rotation file, overrides, auth copying, and shared
  Codex resource links. Internally this is split into config, rotation, and
  shared resource materialization.
- `target`: named target-specific slot groups, Codex config overrides, and env
  overlays from `targets/<name>.toml`.
- `auth`: `auth.json` parsing.
- `usage`: direct usage endpoint requests, payload interpretation, and response
  scoring.
- `selector`: parallel query orchestration and final slot choice.
- `stats`: local usage reporting. Internally this separates SQLite reads,
  rollout calibration, pricing/cache handling, and aggregation.
- `sqlite_merge`: consolidation of legacy per-slot state, memories,
  and goals into the shared `~/.codex/sqlite` directory, followed by cleanup of
  the successfully retired database files.
- `runtime_provider`: maps each slot's concrete provider configuration onto the
  stable `cx` runtime identity used in persisted conversations.
- `desktop_proxy`: proxies the bundled app-server so Desktop lists all providers.
- `envfile`: small parser/writer for `env.conf`.
- `install`: copy the current `cx` binary into a bin directory.
- `upgrade`: one-time startup repairs for profile-manager layouts created by
  already-published versions. See [Startup Upgrade Repairs](upgrades.md).

## Usage Endpoint

Codex's backend client sends:

```text
GET {chatgpt_base_url}/wham/usage
```

for the default ChatGPT backend path. The default normalized base URL is:

```text
https://chatgpt.com/backend-api
```

Required headers come from the local `auth.json` token state:

```text
Authorization: Bearer <tokens.access_token>
ChatGPT-Account-ID: <tokens.account_id or tokens.id_token.chatgpt_account_id>
X-OpenAI-Fedramp: true   # only for FedRAMP accounts
```

The selector treats a slot as exhausted only when the response says:

```text
rate_limit.allowed == false
```

or:

```text
rate_limit.limit_reached == true
```

It intentionally ignores `credits.has_credits=false` as an exhaustion signal.

Available ChatGPT slots keep a score equal to their bottleneck capacity:

```text
min(5h remaining, weekly remaining)
```

Automatic launch selection treats that score as current capacity, then applies
a refresh-aware policy. Slots with at least 20% bottleneck capacity are eligible
for the expected next session; among those, cx chooses the slot whose bottleneck
window refreshes first, then breaks ties by higher bottleneck capacity and
rotation order. If every available slot is below 20%, cx falls back to highest
bottleneck capacity so it does not select a nearly empty account only because it
refreshes soon.

## Usage Refresh Control

Usage refresh is cached per slot, not per `status` command. Fresh cache entries
are valid for 30 seconds. Live refreshes go through a persisted adaptive pacer:
successful refreshes recover the request interval additively, while observed
`429` responses double the interval and set a short cooldown. `Retry-After`
wins over the local fallback cooldown. During cooldown, cx does not start new
usage requests and falls back to stale per-slot cache entries for up to 10
minutes.

## Local Launch

For normal interactive launches, `cx` selects a slot and then uses `exec` on
Unix so the final process is the real Codex binary with:

- `CODEX_HOME=<slot>/home`
- variables from `<slot>/env.conf`
- repeated `-c <line>` arguments from `<slot>/overrides.conf`
- variables from `targets/<target>.toml` `[env]`, when `--target` is used
- repeated target `set`/`overrides` lines, appended after slot overrides
- the user's original Codex args

Authentication selection is owned by cx before `exec`. A locally usable
`auth.json` is primary. Its structure is checked against the current Codex auth
modes and required credential fields. The Keychain PAT credential is read only
when `auth.json` is missing, malformed, mismatched with its declared auth mode,
or contains an expired ChatGPT access token without a refresh token. When the
fallback is selected, cx injects the PAT as `CODEX_ACCESS_TOKEN`. It does not
perform a live OAuth validity probe on the launch path; Codex owns OAuth
refresh. When `auth.json` is selected, inherited auth environment variables are
masked, while competing slot/target auth variables are rejected.

Resume arguments follow the same path: CX forwards the original thread ID
unchanged, and shared SQLite lets Codex resolve the canonical rollout path. CX
does not copy or rewrite rollout JSONL files during resume.

Target-specific execution resolves in this order:

1. `--slot` wins when supplied.
2. `--target <name>` reads `targets/<name>.toml`.
3. If the target has `slots`, only those slots are queried.
4. If the target has no `slots`, `rotation.txt` is queried.
5. Slot env/overrides are loaded first; target env/overrides are applied after
   them so target policy wins over slot defaults.

The real Codex binary is resolved in this order:

1. `--codex-bin`
2. `CX_CODEX_BIN`
3. `mise which codex`
4. `~/.local/share/mise/installs/codex/0.125.0/codex`
5. `codex` from `PATH`

When stdin is piped, `cx` reads it into memory and turns it into:

```text
<prompt>

<stdin>
...
</stdin>
```

It then reattaches the TUI to `/dev/tty`, using `script` when available.
