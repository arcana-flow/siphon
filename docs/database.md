# Database design draft

Use a new local SQLite database. Assign stable local IDs to creators and uploads so users can reference them in CLI commands.

Download presets and the selected default are stored in SQLite and managed through the `preset` commands. See [the decision record](adr/0001-store-download-presets-in-sqlite.md).

| Table | Stored data | Constraints |
| --- | --- | --- |
| `creators` | Local ID, platform, platform creator ID, display name | Unique platform and platform creator ID |
| `follows` | Local ID, creator ID, follow time, refresh checkpoint | Fresh durable local ID (`INTEGER PRIMARY KEY AUTOINCREMENT`, never reused). One active follow per creator. |
| `uploads` | Local ID, creator ID, platform, platform upload ID, URL, title, publication time, status | Unique platform and platform upload ID. Status is `pending`, `downloaded`, or `ignored`. |
| `download_presets` | Preset name, ordered list of arguments | Unique name. Reserve `video` for the built-in preset. |
| `download_preferences` | Selected default preset | One active default. The built-in `video` preset is the initial default. |
| `refresh_preferences` | Optional default cookie source | Store a file path or browser profile selector. Cookie values stay in the selected source. |
| `ownership_sequence` | Monotonic fencing token counter | Singleton row generating strictly increasing ownership tokens inside immediate transactions. |
| `jobs` | Local ID, kind, display-safe target, state, timing, progress JSON, outcome JSON, fencing token, process owner | `INTEGER PRIMARY KEY AUTOINCREMENT`. States are `running`, `succeeded`, `failed`, `interrupted`. Never stores resolved sensitive inputs or raw URL credentials. |
| `operation_reservations` | Slot key, operation kind, upload ID, execution mode, optional job ID, fencing token, process owner, acquisition time | Primary key on `slot_key` (`refresh` or `upload_download:<id>`). Foreground reservations omit `job_id`; background reservations link to `jobs(id)`. |

Unfollowing removes the active follow. The creator and its uploads remain available. Following the same creator again creates a new follow with a fresh durable follow ID (never reusing prior IDs) and initializes its checkpoint to the new follow time.

After a complete refresh for a creator, insert any newly discovered uploads and advance the checkpoint in the same transaction. Existing uploads retain their status. Verify by durable follow ID and checkpoint (alongside follow time) that the active follow still matches the one used for fetching before committing the refresh, so a same-second unfollow and re-follow is always rejected as stale.

For a download tied to a pending upload, update its status only after the downloader succeeds and Siphon confirms an output artifact for that upload.

Exclusive concurrency slots (`refresh` per database and `upload_download` per upload) are shared by foreground and background operations in `operation_reservations`; direct URL downloads take no slot. When a previous owner process is confirmed gone, conditional reconciliation matches the observed fencing token so stale writers or racing reconcilers never mutate newer ownership.
