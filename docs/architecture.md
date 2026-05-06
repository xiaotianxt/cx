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
- `app_server`: minimal loopback WebSocket transport and protocol probe client
  for Codex app-server.
- `control`: private local Unix socket daemon and versioned JSON line protocol
  for future cx-owned adapters.
- `serve`: foreground app-server supervision, lifecycle state, stop, and
  readiness probe.
- `session`: cx-owned session identifiers, channel identifiers, persistent
  session registry, and append-only event journal.
- `protocol_export`: delegates version-matched app-server schema export to the
  selected Codex binary.
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
- variables from `targets/<target>.toml` `[env]`, when `--target` is used
- repeated target `set`/`overrides` lines, appended after slot overrides
- the user's original Codex args

Before `exec`, a clean launch with no prompt, pipe, or forwarded Codex args
passes through an auto-resume policy. `cx` reads the selected slot's shared
`state_5.sqlite`, finds the latest unarchived `cli` or `vscode` thread for the
process cwd, and checks the thread's rollout file with `lsof`. If the file is
not open, `cx` appends `resume <session-id>` after slot and target overrides. If
the file is open, missing, or the active-state probe is inconclusive, `cx`
leaves the arguments unchanged and Codex starts a new session. Explicit Codex
subcommands, help/version, and `--remote` launches bypass this policy.

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

## App Server Control Plane Direction

The current `serve` implementation is intentionally a foreground foundation. It
selects a slot through the same runtime policy as the launcher, spawns:

```text
codex app-server --listen ws://127.0.0.1:<port>
```

and records only non-secret process state under:

```text
<profile-manager>/serve/default.json
```

The first invariant is local containment: `cx serve start` and `cx serve probe`
accept only loopback `ws://127.0.0.1:<port>` or `ws://localhost:<port>` URLs.
Raw app-server WebSocket endpoints are not a stable public control surface for
Telegram, WeChat, remote terminals, or future web adapters.

The second invariant is process ownership: the state file is an observation of
one cx-started process, not a service registry. `cx serve stop` treats a missing
process or dead ready endpoint as stale state and removes only the state file.
It sends signals only after the recorded pid still looks like an app-server
process and the ready endpoint is still live.

The intended staged direction is:

1. Keep raw Codex app-server private to cx.
2. Make process lifecycle explicit: status, stop, stale cleanup, and supervised
   restart behavior.
3. Add a cx-owned daemon/control socket. The current socket only supports
   `ping`, `shutdown`, and session registry commands; it exists to establish the
   private local transport and protocol versioning before turn semantics are
   added.
4. Persist a session registry and event journal. The current registry stores
   cx-owned `session_id`, `primary_channel_id`, `current_channel_id`,
   `lease_epoch`, optional `active_lease`, and timestamps. The journal currently
   records `session-created`, `lease-acquired`, and `lease-released` events as
   NDJSON.
   Lease acquisition refuses to overwrite an active lease unless the caller
   explicitly asks to steal it, which increments the epoch and creates a new
   fencing token. Event listing is exposed through the same control socket so
   adapters do not read cx-owned files directly.
5. Add lease tokens for terminal, Telegram, WeChat, and other channel owners.
6. Route approvals through cx-owned policy before any adapter sees them.
7. Rotate slots by starting or moving work through cx state, not by letting
   adapters connect directly to Codex app-server.

Future channel adapters should use a cx API that carries `session_id`,
`thread_id`, `turn_id`, and a lease token. They should not be handed the raw
app-server address.
