# Telegram Service Test Plan

This plan verifies `cx service` as the one-command runtime for the Codex
app-server plus Telegram adapter.

## Setup

Install the Telegram bot token into the local service token store from any
secret provider or shell command:

```bash
secret-tool-read-telegram-token | cx service token set telegram
cx service token status
```

The token value is read from stdin. `cx` does not know which secret provider
produced it.

Start the service:

```bash
cx service start --acquire-lease
cx service status
cx service logs --lines 80
```

Expected:

- `status` reports `running`.
- Children include `serve` and `telegram`.
- Logs show `cx serve ready` and `telegram child started`.

## Private Chat

1. Send `/start` to the bot.
2. Send `/status`.
3. Send `/new private-a`.
4. Send a short ordinary text prompt.
5. Send `/sessions`.
6. Send `/use private-a`.

Expected:

- `/start` binds the private chat route.
- Ordinary text gets a Codex assistant response.
- `/sessions` lists the default route and `private-a`.
- `/status` reports the active alias and route.

## Basic Group

1. Add the bot to a normal Telegram group.
2. Bind the group if needed.
3. Send `/new group-a`.
4. Send ordinary text.
5. Send `/sessions`.

Expected:

- The group `chat_id` has its own Telegram route.
- Group sessions do not replace private-chat sessions.
- Ordinary text replies land in the group.

## Forum Group Topics

1. Add the bot to a Telegram forum group.
2. In topic A, send `/new topic-a`.
3. In topic B, send `/new topic-b`.
4. Send ordinary text in each topic.
5. Send `/status` in each topic.
6. Send `/sessions` from either topic.

Expected:

- Topic A route includes topic A's `message_thread_id`.
- Topic B route includes topic B's `message_thread_id`.
- Replies stay in the originating topic.
- `/status` in each topic shows that topic's active session.
- `/sessions` lists Telegram-bound sessions for the group chat.

## Multiple Named Sessions

In the same private chat, group, or topic:

```text
/new build
/new review
/use build
What is the current task?
/use review
What is the current task?
/close build
/sessions
```

Expected:

- `build` and `review` keep separate app-server threads.
- `/use` switches only the current route.
- `/close build` unbinds that named session without deleting unrelated routes.

## Broadcast Channel

1. Add the bot as a channel admin.
2. Post `/start` or `/new channel-a` as a channel post.
3. Post ordinary text.

Expected:

- Channel posts are accepted through `channel_post` updates.
- The channel `chat_id` becomes a distinct route.
- Replies use the channel route. If Telegram rejects bot replies because of
  channel permissions, `cx service logs` shows the Telegram API error.

## Service Lifecycle

Run:

```bash
cx service stop --force
cx service status
cx service start --acquire-lease
cx service status
```

Expected:

- Stop removes the service state and terminates recorded children.
- Restart creates fresh `serve` and `telegram` child pids.
- Existing Telegram bindings are preserved.

## macOS launchd

After the private-chat and group tests pass:

```bash
cx service install --start
cx service status
cx service uninstall
```

Expected:

- The launchd plist contains no token value.
- The launchd plist contains no secret-provider command or reference.
- The service starts from the local token store.

## Cleanup

```bash
cx service stop --force
cx service token delete telegram
cx service status
```

Expected:

- Service state is missing or stopped.
- Token status reports missing.
