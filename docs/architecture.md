# Architecture

cx keeps the existing local Codex profile-manager layout and replaces shell
wrappers with a single Rust binary.

## Modules

- `cli`: typed clap commands for management operations.
- `cx`: default launcher and stdin pipe wrapper.
- `run`: argument parsing and `exec` into the real Codex binary.
- `slot`: slot layout, rotation file, overrides, auth copying, shared sqlite links.
  Internally this is split into config, rotation, and sqlite repair helpers.
- `target`: named target-specific slot groups, Codex config overrides, and env
  overlays from `targets/<name>.toml`.
- `auth`: `auth.json` parsing.
- `usage`: direct usage endpoint requests, payload interpretation, and response
  scoring.
- `selector`: parallel query orchestration and final slot choice.
- `stats`: local usage reporting. Internally this separates SQLite reads,
  rollout calibration, pricing/cache handling, and aggregation.
- `envfile`: small parser/writer for `env.conf`.
- `install`: copy the current `cx` binary into a bin directory.

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

## Local Launch

For normal interactive launches, `cx` selects a slot and then uses `exec` on
Unix so the final process is the real Codex binary with:

- `CODEX_HOME=<slot>/home`
- variables from `<slot>/env.conf`
- repeated `-c <line>` arguments from `<slot>/overrides.conf`
- variables from `targets/<target>.toml` `[env]`, when `--target` is used
- repeated target `set`/`overrides` lines, appended after slot overrides
- the user's original Codex args

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
