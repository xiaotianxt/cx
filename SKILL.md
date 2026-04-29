# cx

Use this skill when you need to operate the local Codex multi-slot workflow.

## Commands

- `cx`: run Codex through the best available slot.
- `cat file | cx "prompt"`: wrap stdin as Codex prompt context.
- `cx status`: show all configured slots and live usage.
- `cx select`: print the best slot name.
- `cx add <slot> --rotate`: create a slot and add it to rotation.
- `cx login <slot>`: log into a slot-specific `CODEX_HOME`.
- `cx doctor --online`: validate local layout and query usage.

## Safety

Do not print `auth.json`, access tokens, or `env.conf` values. These files may
contain live credentials.
