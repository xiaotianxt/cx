---
name: cx
description: Use when Codex needs to operate the local Codex multi-slot workflow with cx, including selecting the best slot, checking usage, creating or logging into slots, or validating slot state.
---

# cx

Canonical source: https://github.com/xiaotianxt/skills/tree/main/skills/cx

Use this skill when you need to operate the local Codex multi-slot workflow.

## Commands

- `cx`: launch Codex through the best available local slot.
- `cat file | cx "prompt"`: wrap stdin as Codex prompt context.
- `cx status`: show all configured slots and live usage.
- `cx stats`: show local token usage totals from Codex state.
- `cx stats --calibrate`: scan rollout token counts and save the price-estimate token mix.
- `cx select`: print the best slot name.
- `cx add <slot> --rotate`: create a slot and add it to rotation.
- `cx login <slot>`: log into a slot-specific `CODEX_HOME`.
- `cx doctor --online`: validate local layout and query usage.

## Safety

Do not print `auth.json`, access tokens, or `env.conf` values. These files may
contain live credentials.
