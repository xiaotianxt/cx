# Startup Upgrade Repairs

cx runs a small startup repair pass before commands that read or mutate the
profile-manager layout. The repair pass is intentionally separate from normal
runtime code: current modules keep accepting only the current file and directory
shape, while `src/upgrade.rs` handles one-time fixes for users moving from
already-published versions.

The repair marker is written to:

```text
<profile-manager>/state/upgrades/runtime-surface-removal-v1.json
```

Set `CX_DISABLE_STARTUP_REPAIR=1` only for debugging a broken local profile.

## Public Version Matrix

The public tags checked for this repair are `v0.1.2` through `v0.4.1`.

| Public versions | User-visible risk after current cleanup | Startup repair |
| --- | --- | --- |
| `v0.1.2` through `v0.4.1` | `price-cache.json` may omit the current `schemaVersion`. | Adds the current schema marker when the file shape matches cx's price cache. |
| `v0.1.6` through `v0.4.1` | `stats-calibration.json` may use the first cx-owned file schema. | Updates the schema marker when the file shape matches cx's calibration cache. |
| Up to `v0.3.7` | Slot homes may contain sqlite files directly under `<slot>/home`. | Moves real sqlite files into `<slot>/home/sqlite`; removes root-level sqlite symlinks. |
| `v0.1.11` through `v0.4.1` | Removed foreground runtime state may remain under `<profile-manager>/serve`. | Moves inactive state into `<profile-manager>/state/retired-runtime/runtime-surface-removal-v1`. |
| `v0.2.3` through `v0.4.1` | Removed background runtime launch state may keep trying to start after the binary is upgraded. | Unloads the old macOS LaunchAgent when present, then retires the plist and `<profile-manager>/service`. |

## Review Notes

- The repair is idempotent and quiet when no local files need work.
- The repair archives removed runtime state instead of deleting it, because those
  directories may contain logs or local token files.
- `cx transfer import` runs the repair after copying a bundle so older bundles
  are upgraded even when the startup marker already exists.
- Normal stats parsing does not accept old files. The repair pass must run
  before stats reads those cx-owned files.
