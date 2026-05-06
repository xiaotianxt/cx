# Telegram Channel Adapter

`cx channel telegram` is the first remote channel adapter for cx. It connects a
Telegram bot to cx's local session, lease, and event journal. It does not start
Codex turns yet.

## Boundaries

- Telegram messages become cx channel events and optional session leases.
- Message text is not stored in cx state or the event journal.
- The Telegram bot token is read from an environment variable and is never
  written to disk.
- Only trusted local bindings and explicitly allowed chat ids are processed.
- A new chat can become trusted only by sending a generated `/bind <secret>`.
- The adapter uses Telegram long polling. It does not expose a public local
  server or webhook.

## State

The adapter writes:

```text
~/.codex/profile-manager/serve/channels/telegram.json
```

The file stores:

```json
{
  "schemaVersion": 1,
  "lastUpdateId": 123456,
  "bindings": [
    {
      "chatId": 12345,
      "channelId": "telegram:12345",
      "sessionId": "sess_abc123"
    }
  ]
}
```

The bot token, message text, prompts, and Codex output are not stored there.

## Run And Bind

Set a bot token in the environment:

```bash
export TELEGRAM_BOT_TOKEN=...
```

Start the adapter:

```bash
cx channel telegram run
```

Plain Telegram text is submitted to Codex only when a foreground app-server is
already running:

```bash
cx serve start
cx channel telegram run
```

Keep both commands running. `cx channel telegram run` reads the saved loopback
app-server state, creates one Codex app-server thread per trusted Telegram
binding, reuses that thread on later messages, and sends the final assistant
text back to Telegram. Long replies are split into Telegram-sized messages.

`run` and `bind` also synchronize the bot's Telegram command menu with:

```text
/start
/bind
/status
/sessions
/release
```

To sync only the menu and exit:

```bash
cx channel telegram menu
```

If no Telegram chats are trusted yet, `run` prints a one-time bind command:

```text
cx telegram onboarding
send this message to the Telegram bot:
/bind <secret>
waiting for first matching chat...
```

Send that `/bind <secret>` message to the bot from Telegram. The adapter records
that `chat.id` as a trusted local binding, creates a cx session for it, and then
continues running. The secret is generated in memory and is not written to the
state file.

After at least one chat is trusted, `run` does not create a new bind secret. It
listens only to trusted bindings and any explicit `--allow-chat` values:

```bash
cx channel telegram run
cx channel telegram run --allow-chat 12345
```

To trust an additional chat, run a one-shot bind:

```bash
cx channel telegram bind
```

`bind` prints a fresh `/bind <secret>`, waits for the first matching Telegram
chat, records it as trusted, then exits.

Use a custom token environment variable:

```bash
CX_TELEGRAM_TOKEN=... \
cx channel telegram run --bot-token-env CX_TELEGRAM_TOKEN
```

Acquire a cx session lease for incoming Telegram messages:

```bash
cx channel telegram run --acquire-lease
```

Allow Telegram to take over an existing lease:

```bash
cx channel telegram run --acquire-lease --steal
```

Without `--steal`, the adapter will reply that the session is controlled
elsewhere when another channel holds the lease.

## Manual Chat Id Debugging

`--allow-chat` must be the Telegram `chat.id`, not the numeric prefix in the bot
token. Most users should use `/bind` instead of discovering chat ids manually.

For debugging, pass `--log-updates` and send a message to the bot:

```bash
cx channel telegram run --allow-chat 0 --poll-timeout 5 --log-updates
```

The adapter prints safe summaries to stderr without token or message text:

```text
telegram update update_id=123 source=message chat_id=12345 text=present allowed=false
```

## Commands In Telegram

The adapter recognizes these Telegram messages:

```text
/start
/bind <secret>
/status
/sessions
/release
```

Behavior:

- `/bind <secret>` trusts a new chat when the secret matches the one printed by
  `run` onboarding or `cx channel telegram bind`.
- `/start` creates or reuses a binding only for already trusted chats.
- `/status` reports the bound session and current lease holder.
- `/sessions` lists cx sessions known to the local profile manager.
- `/release` releases the Telegram lease when Telegram currently holds it.
- Any other text creates or reuses a binding, records a
  `channel-message-received` event without storing the text, then submits the
  text to the running Codex app-server and replies with the assistant's final
  message.

## Inspect Locally

Show Telegram adapter state:

```bash
cx channel telegram status
cx channel telegram status --json
```

Inspect cx sessions and events:

```bash
cx serve session list --json
cx serve event list --json
```

The adapter writes local channel/session state directly and uses `cx serve
start` only for Codex app-server turns. The older `cx serve daemon` control
socket is not required for Telegram.

## Current Limits

This first version intentionally does not:

- Stream partial Codex output back to Telegram while the turn is running.
- Start or stop `codex app-server` automatically.
- Rotate Codex accounts on rate limits.
- Expose a public WebSocket server.

Those pieces should build on top of this channel/session/lease/app-server
foundation.
