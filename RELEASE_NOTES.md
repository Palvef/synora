# Synora 0.1.7

## Fixes

- `tunasync.json` now matches TUNA / mirror-web: only `success`,
  `syncing`, `failed`, and `paused`. Idle `queued` / `cancelled` jobs keep
  their last finished result instead of rendering as unknown in
  ha-mirrors-web
- `last_update` is the last successful sync (not the last failed run)
- sizes stay `du -h` style (`1.6T`, `115.6G`); timestamps stay
  `2026-08-22 07:13:00 +0800`
- status collection no longer issues one DB query per job

## Notes

- Native `synora.json` still exposes internal states such as `queued`
