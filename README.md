# Siphon

Siphon is a Rust CLI rewrite of `mrm`.

## v1 scope

- Follow Bilibili creators and refresh manually to discover new uploads. Download pending uploads or mark them as ignored.
- A new follow starts at the time the creator is followed. The first refresh discovers uploads published after that time.
- Unfollowing stops future refreshes and keeps previously discovered uploads and their statuses. Following the creator again starts a new refresh window at the new follow time.
- Refresh each creator independently. Fetch all pages up to the previous refresh boundary, or until the platform returns no more uploads, then save the discovered uploads and advance the checkpoint in one database transaction. A failed refresh leaves that creator's checkpoint unchanged and allows other creators to finish.
- Identify each upload by its platform and platform-assigned ID. Refreshes preserve the status of known uploads, even if their URLs change. A reupload with a new platform ID is a separate upload.
- Download media directly with `siphon download <URL>` without following its creator.
- A download started from a pending upload marks it as downloaded only if `yt-dlp` succeeds and Siphon confirms an output artifact for that upload. A simulation leaves it pending. A subtitle-only download counts when it saves a subtitle. Direct downloads leave upload status unchanged, even when the URL matches a pending upload.
- Direct downloads follow `yt-dlp` playlist behavior, including options supplied through presets or after `--`. `siphon download --upload <ID>` is limited to the selected upload.
- Save multiple named download presets, each with its own arguments, and select one as the default. `yt-dlp` is the sole download tool in v1.
- The built-in `video` preset is the initial default and downloads video with audio. Install `yt-dlp` to use it. You can select another preset as the default.
- Pass one-off arguments to `yt-dlp` after `--` in `siphon download <URL> -- <yt-dlp-args>`.
- Siphon passes preset arguments first, then appends the one-off arguments in the order supplied. Use `--no-preset` before `--` to omit all preset arguments for that invocation.
- Siphon starts with an empty database and uses a new schema designed for the CLI. v1 has no import from the `mrm` database.

## CLI commands

| Command | Purpose |
| --- | --- |
| `siphon creator add <URL>` | Follow a Bilibili creator. |
| `siphon creator list` | List followed creators. |
| `siphon creator remove <ID>` | Unfollow a creator. |
| `siphon refresh [CREATOR_ID]` | Refresh one creator or all followed creators. |
| `siphon upload list` | List pending uploads. |
| `siphon upload ignore <ID>` | Mark an upload as ignored. |
| `siphon download <URL>` | Download directly from a URL. |
| `siphon download --upload <ID>` | Download a pending upload and update its status on success. |
| `siphon preset add <NAME> -- <ARGS>` | Save a named download preset. |
| `siphon preset list` | List download presets. |
| `siphon preset remove <NAME>` | Remove a download preset. |
| `siphon preset set-default <NAME>` | Set the default download preset. |

Use `--preset <NAME>` on either download command to select a preset for that invocation.

## Cookies

Both `refresh` and `download` accept cookies from a Netscape-format cookie file or a browser profile. Refresh uses the selected cookies for Bilibili requests. Download makes the selected cookies available to `yt-dlp`. See the [yt-dlp cookie options](https://github.com/yt-dlp/yt-dlp#filesystem-options).

Refresh can remember a default cookie source. Downloads use a cookie source only when explicitly selected for that invocation. The refresh default does not carry over to downloads.

## Continuous integration

GitHub Actions runs on pull requests and pushes to the default branch. It checks formatting with `cargo fmt --all -- --check` and runs `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` on Linux, macOS, and Windows.

CI tests use local fixtures and a stub downloader so they can verify refresh, argument forwarding, and download status without live accounts or external services.

## Design notes

See the [database design draft](docs/database.md).
