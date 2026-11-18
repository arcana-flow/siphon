# Database design draft

Use a new local SQLite database. Assign stable local IDs to creators and uploads so users can reference them in CLI commands.

Download presets and the selected default are stored in SQLite and managed through the `preset` commands. See [the decision record](adr/0001-store-download-presets-in-sqlite.md).

| Table | Stored data | Constraints |
| --- | --- | --- |
| `creators` | Local ID, platform, platform creator ID, display name | Unique platform and platform creator ID |
| `follows` | Creator ID, follow time, refresh checkpoint | One active follow per creator |
| `uploads` | Local ID, creator ID, platform, platform upload ID, URL, title, publication time, status | Unique platform and platform upload ID. Status is `pending`, `downloaded`, or `ignored`. |
| `download_presets` | Preset name, ordered list of arguments | Unique name. Reserve `video` for the built-in preset. |
| `download_preferences` | Selected default preset | One active default. The built-in `video` preset is the initial default. |
| `refresh_preferences` | Optional default cookie source | Store a file path or browser profile selector. Cookie values stay in the selected source. |

Unfollowing removes the active follow. The creator and its uploads remain available. Following the same creator again creates a new follow and initializes its checkpoint to the new follow time.

After a complete refresh for a creator, insert any newly discovered uploads and advance the checkpoint in the same transaction. Existing uploads retain their status. Verify that the active follow and checkpoint still match the ones used for fetching before committing the refresh.

For a download tied to a pending upload, update its status only after the downloader succeeds and Siphon confirms an output artifact for that upload.
