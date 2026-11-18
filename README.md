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

Use `--preset <NAME>` on either download command to select a preset for that invocation, or `--db <PATH>` (or `SIPHON_DB`) on any command to override the default per-user SQLite database path (`$XDG_DATA_HOME/siphon/siphon.db` or `~/.local/share/siphon/siphon.db` on Linux, `~/Library/Application Support/siphon/siphon.db` on macOS, `%LOCALAPPDATA%\siphon\siphon.db` on Windows).

`siphon download` routes Siphon status summaries to `stderr` so `yt-dlp` `stdout` (such as `-J` or `--print`) passes through uncontaminated.

## Cookies

Both `refresh` and `download` accept `--cookies <FILE>` (a Netscape-format cookie file) or `--cookies-from-browser <BROWSER>` (`yt-dlp`-compatible browser profile selector). Refresh uses the selected cookies for Bilibili requests. Download makes the selected cookies available to `yt-dlp`. See the [yt-dlp cookie options](https://github.com/yt-dlp/yt-dlp#filesystem-options).

Refresh can remember a default cookie source with `--save-cookies`, clear it with `--clear-cookies`, or bypass it for one run with `--no-cookies`. Downloads use a cookie source only when explicitly selected for that invocation. The refresh default does not carry over to downloads.

## Installation and releases

Pre-built standalone binary archives are published on [GitHub Releases](https://github.com/arcana-flow/siphon/releases) for stable release tags (`vX.Y.Z`).

### Supported platforms and baselines

| Platform | Architecture | Archive | Target | Platform baseline |
| --- | --- | --- | --- | --- |
| Linux | x86_64 | `siphon-v<version>-linux-x86_64.tar.gz` | `x86_64-unknown-linux-gnu` | glibc 2.35+ (Ubuntu 22.04 LTS+) |
| macOS | x86_64 (Intel) | `siphon-v<version>-macos-x86_64.tar.gz` | `x86_64-apple-darwin` | macOS 13+ |
| macOS | aarch64 (Apple Silicon) | `siphon-v<version>-macos-aarch64.tar.gz` | `aarch64-apple-darwin` | macOS 14+ |
| Windows | x86_64 | `siphon-v<version>-windows-x86_64.zip` | `x86_64-pc-windows-msvc` | Windows 10+ / Windows Server 2022+ |

Every release includes a single `SHA256SUMS` file covering all four distribution archives.

### Verifying downloads

Download the archive for your platform alongside `SHA256SUMS` from the release page, then verify checksum integrity:

- **Linux**:
  ```sh
  sha256sum -c SHA256SUMS --ignore-missing
  ```
- **macOS**:
  ```sh
  shasum -a 256 -c SHA256SUMS --ignore-missing
  ```
- **Windows (PowerShell)**:
  ```powershell
  Get-FileHash siphon-v*-windows-x86_64.zip -Algorithm SHA256
  ```

### Extracting and installing

- **Linux / macOS**:
  Extract the archive and move `siphon` into a directory on your `PATH` (such as `/usr/local/bin` or `~/.local/bin`):
  ```sh
  tar -xzf siphon-v<version>-<os>-<arch>.tar.gz
  chmod +x siphon
  mv siphon ~/.local/bin/
  ```
- **Windows**:
  Extract `siphon-v<version>-windows-x86_64.zip` and place `siphon.exe` into a folder included in your `PATH`.

### Dependency: yt-dlp

Siphon invokes `yt-dlp` to execute media downloads (`siphon download`). Siphon does not bundle or manage `yt-dlp`; users manage and update `yt-dlp` independently so that the executable is available in `PATH`:

- **macOS (Homebrew)**: `brew install yt-dlp`
- **Linux**: Install through your package manager (e.g. `sudo apt install yt-dlp`) or via `pipx install yt-dlp`
- **Windows**: `winget install yt-dlp` or `choco install yt-dlp`

Verify `yt-dlp` is available before running downloads:
```sh
yt-dlp --version
```

## Continuous integration and release delivery

GitHub Actions runs continuous integration on pull requests and pushes to `master`. It checks formatting with `cargo fmt --all -- --check` and runs `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` on Linux, macOS, and Windows.

CI tests use local fixtures and a stub downloader so they can verify refresh, argument forwarding, and download status without live accounts or external services.

Pushing a stable release tag (`vX.Y.Z`) triggers the GitHub Release CD workflow. It validates tag format, package version match, and commit ancestry on `origin/master`, repeats formatting, Clippy, and test checks, builds locked release binaries across all four target architectures on native runners, performs CLI smoke checks, and publishes draft releases with autogenerated notes and checksums once all assets are attached.

## Design notes

See the [database design draft](docs/database.md).
