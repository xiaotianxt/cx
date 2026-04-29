# Architecture

cx keeps the existing local Codex profile-manager layout and replaces shell
wrappers with a single Rust binary.

## Modules

- `cli`: typed clap commands for management operations.
- `cx`: default launcher and stdin pipe wrapper.
- `run`: argument parsing and `exec` into the real Codex binary.
- `slot`: slot layout, rotation file, overrides, auth copying, shared sqlite links.
- `auth`: `auth.json` parsing.
- `usage`: direct usage endpoint requests and response scoring.
- `selector`: parallel query orchestration and final slot choice.
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

## Exec Model

`cx` does not spawn a long-running child wrapper. On Unix it uses `exec`, so the
final process is the real Codex binary with:

- `CODEX_HOME=<slot>/home`
- variables from `<slot>/env.conf`
- repeated `-c <line>` arguments from `<slot>/overrides.conf`
- the user's original Codex args

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
