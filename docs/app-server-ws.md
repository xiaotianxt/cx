# Codex App Server WebSocket

This document is the hands-on reference for the app-server path in cx. It covers
the private Codex App Server WebSocket, the cx commands that exercise it, and the
boundary between that private protocol and cx's own control-plane state.

## Boundaries

- `cx serve start` launches `codex app-server` through cx slot and target
  selection.
- `cx serve probe` verifies the WebSocket handshake and a read-only
  `thread/list` call.
- `cx serve threads` lists Codex app-server threads through cx's adapter.
- `cx protocol export` exports the version-matched Codex schema and TypeScript
  bindings from the selected Codex binary.
- `cx serve daemon/session/lease/event` is cx's local Unix-socket control plane.
  It manages cx-owned sessions and channel leases. It is not the Codex
  app-server WebSocket.

The app-server listener is loopback-only in cx. Keep it on
`ws://127.0.0.1:<port>` or `ws://localhost:<port>`. For remote use, tunnel the
loopback port with SSH instead of binding the app-server to a public interface.

## Start A Server

Start a foreground Codex app-server on a selected slot:

```bash
cx serve start --slot primary --listen ws://127.0.0.1:17654
```

Or let cx choose the slot from rotation or a target:

```bash
cx serve start
cx serve start --target research
```

When `--listen` uses port `0`, cx reserves a concrete loopback port before
starting Codex and records the final URL in:

```text
~/.codex/profile-manager/serve/default.json
```

That state file contains the pid, slot, optional target, WebSocket URL, `/readyz`
URL, and start time. It does not contain auth tokens, environment values,
prompts, or Codex output.

## Check Readiness

Use cx's state file:

```bash
cx serve status
cx serve status --json
```

Probe the actual app-server protocol path:

```bash
cx serve probe
cx serve probe --listen ws://127.0.0.1:17654 --json
```

`probe` connects to the WebSocket, sends `initialize`, then sends a one-row
`thread/list` request with `useStateDbOnly: true`. It does not start a model
turn.

## List Threads

List threads through cx's app-server adapter:

```bash
cx serve threads
cx serve threads --limit 20 --json
cx serve threads --listen ws://127.0.0.1:17654 --limit 5 --json
```

The JSON output is intentionally cx-shaped. It includes the selected slot and
target from cx state when available, plus thread summaries mapped out of the
private Codex response:

```json
{
  "schemaVersion": 1,
  "listenUrl": "ws://127.0.0.1:17654",
  "ready": true,
  "slot": "primary",
  "target": null,
  "threads": [
    {
      "upstreamThreadId": "thread-id-from-codex",
      "title": "Fix build",
      "preview": "please fix the failing test",
      "cwd": "/path/to/repo",
      "source": "cli",
      "status": "idle",
      "active": false,
      "createdAtUnix": 1800000000,
      "updatedAtUnix": 1800000300
    }
  ]
}
```

## Raw WebSocket Shape

Codex App Server currently uses a JSON-RPC-like envelope without the
`"jsonrpc": "2.0"` field. cx keeps this private shape inside `src/app_server`.

Initialize:

```json
{
  "id": 1,
  "method": "initialize",
  "params": {
    "clientInfo": {
      "name": "my-client",
      "version": "0.1.0"
    },
    "capabilities": {}
  }
}
```

List threads:

```json
{
  "id": 2,
  "method": "thread/list",
  "params": {
    "limit": 20,
    "useStateDbOnly": true
  }
}
```

Responses have the same `id` and either `result` or `error`:

```json
{
  "id": 2,
  "result": {
    "data": [],
    "nextCursor": null,
    "backwardsCursor": null
  }
}
```

Server notifications are uncorrelated objects with `method` and optional
`params`. A client waiting for a request response must ignore notifications until
it receives the matching response id.

## Export Schemas

Export the protocol definitions from the exact Codex binary cx would use:

```bash
cx protocol export --out /tmp/codex-app-protocol
cx protocol export --out /tmp/codex-app-protocol --json-schema
cx protocol export --out /tmp/codex-app-protocol --typescript
```

The output directories are:

```text
/tmp/codex-app-protocol/json-schema/
/tmp/codex-app-protocol/typescript/
```

Use these exported files as the source of truth for downstream clients that need
full coverage of Codex private methods. Use cx commands for stable smoke tests
and for the subset that cx has intentionally wrapped.

## Stop The Server

Stop the recorded foreground app-server:

```bash
cx serve stop
cx serve stop --force --json
```

If the recorded process no longer matches a live ready app-server, cx only
cleans the stale state file.
