# Telegram Channel Adapter

`cx channel telegram` is the first remote channel adapter for cx. It connects a
Telegram bot to cx's local session, lease, event journal, and Codex app-server
turns.

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
      "messageThreadId": 99,
      "alias": "build",
      "channelId": "telegram:12345",
      "sessionId": "sess_abc123",
      "appThreadId": "019d..."
    }
  ],
  "activeRoutes": [
    {
      "chatId": 12345,
      "messageThreadId": 99,
      "alias": "build"
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

To run both pieces under one background supervisor instead:

```bash
secret-tool-read-telegram-token | cx service token set telegram
cx service start
cx service status
cx service logs
cx service stop
```

`cx service start` starts `cx serve start`, waits for app-server state, then
starts `cx channel telegram run`. It defaults to Telegram; pass `--no-telegram`
when only a background app-server is wanted. `cx service token set telegram`
reads the token from stdin and stores it with private file permissions; the
start command and launchd plist do not carry the token. For login startup on
macOS:

```bash
cx service install --start
```

`run` and `bind` also keep the bot's Telegram command menu intentionally small:

```text
/start
/bind
/portal
/status
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

## Routing Model

The adapter routes messages by Telegram route scope:

```text
telegram:<chat_id>
telegram:<chat_id>:topic:<message_thread_id>
telegram:<chat_id>:topic:<message_thread_id>:session:<alias>
```

Private chats normally use one default route. Telegram forum groups use the
chat-level or General topic route as the portal/admin surface, and work topics
as handoff rooms. A work topic binds to one cx session and one Codex app-server
thread. Topic titles are display metadata; the stable identity is the Telegram
`chat_id` plus `message_thread_id`.

Binding the chat-level group route also trusts topic routes inside that group,
so one group bind is enough before creating per-topic handoff rooms.
The bot API can create topics only inside an existing forum supergroup where
the bot is an administrator with topic-management permission; it cannot create
the supergroup itself. Create the forum group in Telegram first, enable Topics,
add the bot, then bind the group or a topic with `/bind <secret>`.

The portal is click-first. Open it with `/portal` when needed, or let service
startup refresh the saved portal panel:

```text
/portal
```

The portal lists recent app-server threads with inline buttons and a Refresh
button. Tapping a thread creates a forum work topic when needed, binds that
topic to the selected Codex thread, and resumes messages in that topic against
the same thread and captured cwd. If the selected thread is active, cx tries to
interrupt the active turn before marking the Telegram handoff active.

Work topics also get a small control panel. The active panel offers Release to
desktop, Refresh, and Close topic. A released panel offers Take over from
desktop, Refresh, and Close topic. Plain text in the portal/router route shows
the portal panel instead of starting a Codex turn.

When `cx channel telegram run` starts, it refreshes locally known portal routes:
it syncs the command menu, tries to unhide and rename the General forum topic
to `cx portal`, and edits the saved portal panel in place. If the saved panel
message no longer exists, it sends a fresh one and records that message id.
Telegram's Bot API does not expose a method for listing every forum topic, so
startup reconciliation is limited to routes already present in local cx state.

Inside any route, named sessions are still available for parallel work:

```text
/new build
/use build
/sessions
/close build
```

Aliases are normalized to lowercase ASCII letters, numbers, `.`, `_`, and `-`,
then capped at 32 characters. If `/new` has no name, cx creates `session-1`,
`session-2`, and so on for that route.

`/close` in a work topic archives the bound Codex app-server thread, asks
Telegram to delete that forum topic, and removes the local binding only after
Telegram accepts the delete request. If Telegram rejects the delete request,
the binding stays active so the adapter can retry after the bot receives the
right admin permissions. In private chats or non-topic routes, `/close` only
unbinds the local route.

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

## Interaction Feedback

For every trusted incoming message, the adapter immediately tries to add a
`eyes` reaction to the original Telegram message. Reaction failures are logged
but do not fail the turn because Telegram can reject reactions depending on chat
permissions or message type.

While a normal text message is being processed by Codex, the adapter sends
Telegram `typing` chat actions in the same private chat, group, channel, or
forum topic. Assistant deltas from the Codex app-server are streamed into one
Telegram message by editing it in place. If the final answer is longer than a
single Telegram message, cx sends the overflow chunks after the turn completes.

If Codex asks for command, file-change, or permission approval during a
Telegram-started turn, cx sends an approval panel into the same route and waits
inside that turn for a button click. Allow once grants only the current request,
Allow session grants the matching request for the app-server session when the
upstream protocol supports it, and Deny returns a rejection to Codex.

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

The visible Telegram command menu is intentionally small:

```text
/start
/bind <secret>
/portal
/status
```

The adapter still accepts these text commands as fallback controls:

```text
/new [name]
/use <name>
/sessions
/close [name]
/release
/takeover
/attach <thread-id>
```

Behavior:

- `/bind <secret>` trusts a new chat when the secret matches the one printed by
  `run` onboarding or `cx channel telegram bind`.
- `/start` creates or reuses a binding only for already trusted chats.
- `/portal` opens the handoff portal with inline buttons for recent local Codex
  app-server threads.
- `/history` is an alias for `/portal`.
- `/attach <thread-id>` binds the current route to a visible app-server thread.
- `/takeover` resumes Telegram control for a paused handoff topic and tries to
  interrupt the active app-server turn.
- `/new [name]` creates a named cx session for the current private chat, group,
  or forum topic route and switches to it.
- `/use <name>` switches the current route to an existing named session.
- `/status` reports the bound session and current lease holder.
- `/sessions` lists Telegram-bound sessions for the current chat.
- `/close [name]` unbinds a named session from the current route. Without a
  name, it unbinds the active session for that route. In a work topic, cx
  archives the bound app-server thread and deletes the Telegram forum topic.
- `/release` pauses Telegram handoff for the topic and releases the Telegram
  lease when Telegram currently holds it. Messages in the topic are held until
  the work panel's Take over button or `/takeover` is used.
- Approval panels are click-only and are scoped to the route and message that
  requested approval. Stale approval button clicks are answered as no longer
  pending.
- Any other text in a work topic creates or reuses a binding, records a
  `channel-message-received` event without storing the text, then submits the
  text to the running Codex app-server and replies with the assistant's final
  message. Any other text in the portal/router route refreshes the portal
  panel instead of starting a Codex turn.

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
