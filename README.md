# DriveSync (`dsync`)

A small, fast Rust CLI, modelled on [odeke-em/drive](https://github.com/odeke-em/drive), that pushes and
pulls a local directory to and from Google Drive and shows a diff of modification times between the two.

- Project: <https://github.com/scaleninja/drivesync>
- License: [MIT](LICENSE)

```
dsync init [DIR] --remote-folder PATH --depth N      # authorize + create .gd/ in DIR
dsync push [PATH] [--force] [-y] [-j N] [--refresh]  # upload local changes (PATH relative to cwd)
dsync pull [PATH] [--force] [-y] [-j N] [--refresh]  # download remote changes
dsync diff [PATH] [--refresh]                        # list files whose local/remote mtimes differ
dsync status                                         # local dir, remote folder, depth, cache, ignore file, token
dsync version
```

## Install

Prebuilt binaries for Linux and macOS (x86_64 and arm64) are attached to each
[GitHub release](https://github.com/scaleninja/drivesync/releases).

```bash
# Linux / macOS: detect the platform and install to /usr/local/bin
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)   asset=linux-x86_64 ;;
  Linux-aarch64)  asset=linux-arm64 ;;
  Darwin-x86_64)  asset=macos-x86_64 ;;
  Darwin-arm64)   asset=macos-arm64 ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)"; exit 1 ;;
esac
curl -fL -o /tmp/dsync "https://github.com/scaleninja/drivesync/releases/latest/download/dsync-$asset" \
  && chmod 755 /tmp/dsync && sudo mv /tmp/dsync /usr/local/bin/dsync
```

Or build from source (needs a Rust toolchain):

```bash
git clone https://github.com/scaleninja/drivesync
cd drivesync
make install          # cargo install --path .  -> ~/.cargo/bin/dsync
```

## Setup

### 1. Obtain OAuth 2.0 credentials

`dsync` talks to Google Drive on your behalf using an OAuth 2.0 client that **you** create. Nothing is
embedded in the binary, so each user (or team) needs to do this once:

1. Go to the [Google Cloud Console](https://console.cloud.google.com/) and sign in.
2. Create a new project (or pick an existing one) from the project selector at the top.
3. Enable the **Google Drive API** for the project:
   - Navigate to **APIs & Services > Library**.
   - Search for "Google Drive API" and click **Enable**.
4. Configure the consent screen (first time only):
   - Navigate to **APIs & Services > OAuth consent screen** (called **Google Auth Platform > Branding** in
     newer consoles).
   - Choose **External** (or **Internal** if you use Google Workspace), give the app a name such as
     `dsync`, and fill in the required support and developer email addresses. The name you pick here is
     what the browser consent page will display.
   - Under **Audience** / **Test users**, add the Google account(s) that will use `dsync`. While the app is
     in *Testing* status only listed test users can authorize it.
5. Create the OAuth client:
   - Navigate to **APIs & Services > Credentials**.
   - Click **Create Credentials > OAuth client ID**.
   - Choose **Desktop app** as the application type and give it any name.
   - Click **Download JSON** and save the file, for example as `~/Downloads/client_secret.json`.

> **Note on Testing status.** Google expires refresh tokens after 7 days for External apps that are still in
> *Testing*. If `dsync` keeps asking you to re-run `dsync init` every week, either publish the app
> (**OAuth consent screen > Publish app**; no verification is needed for personal use) or use an
> *Internal* app on a Google Workspace account.

### 2. Initialize a sync folder

```bash
dsync init ~/gdrive --remote-folder backups/lab --credentials ~/Downloads/client_secret.json
# or: GOOGLE_CLIENT_ID=... GOOGLE_CLIENT_SECRET=... dsync init ~/gdrive
```

A browser opens for consent; the CLI receives the code on a loopback port and stores the tokens in
`~/gdrive/.gd/`. The remote folder is created under *My Drive* if missing. You can delete
`client_secret.json` afterwards; `dsync` keeps its own copy of the id and secret in `.gd/config.json`.

`.gd/` holds your client secret and refresh token with mode `0600`. It is never uploaded by `dsync`, but if
the sync folder is also a git repository add `.gd/` to that repository's `.gitignore`.

### 3. Sync

```bash
cd ~/gdrive
dsync status
dsync diff
dsync push          # shows the plan, asks "Proceed with the changes? [Y/n]"
dsync pull -y -j 16 # no prompt, 16 parallel streams
```

## How it works

- `.gd/config.json` stores the client credentials, remote folder path/id and depth.
  `.gd/credentials.json` stores the access and refresh tokens.
- **Token refresh**: the access token is refreshed automatically when it is within 60 s of expiry, and again
  if the API ever returns 401. Worker threads share one token; only the first 401 triggers a refresh.
- **Comparison**: a file is unchanged if the MD5 matches; otherwise the newer modification time wins
  (1 s tolerance). Uploads set Drive's `modifiedTime` to the local mtime, and downloads set the local mtime
  to Drive's `modifiedTime`, so `dsync diff` is clean after a sync.
- **Confirmation**: push and pull first print the planned changes (mkdir / upload / update / download and
  any skips) and ask `Proceed with the changes? [Y/n]`. Pass `--no-prompt` (`-y`) for scripts.
- **Parallelism**: folders are created first, then file transfers (push and pull alike) run on
  `--threads` (`-j`) workers, default 8, max 64. A spinner shows progress while scanning and transferring.
  One failed file does not stop the others; the exit code is non-zero if any failed.
- **Transfers**: files up to 5 MB go in a single multipart request; larger files stream through a
  resumable upload session, and downloads stream to a temp file that is renamed into place. Rate-limit
  (403/429), server (5xx) and network errors are retried with exponential backoff.
- **Safety**: push skips files where remote is newer, pull skips files where local is newer, unless `--force`.
  Nothing is ever deleted on either side. Google-native docs (Docs/Sheets/...) are listed but not downloaded.
- **Cache**: `.gd/cache.db` is a SQLite index of the remote tree (path, id, mtime, md5). It is opened in
  WAL mode with a busy timeout, so several CLI instances can read and write it concurrently. The first run
  lists the remote tree in full and records a Drive Changes API token; later runs fetch only the changes
  since that token and patch the index, so repeated push/pull/diff runs do not re-walk Drive.
  `--refresh` forces a full re-listing (also done automatically if `depth` changed or the incremental
  update fails).
- **Lock**: push and pull hold an exclusive lock on `.gd/lock`; a second instance waits for the first.
- **Ignore**: `.driveignore` in the sync root uses gitignore syntax. `.gd/` is always ignored.
- **Paths**: `PATH` is relative to the current directory and may be a file or a directory inside the sync root.
  Depth applies from that path.

## Building

```
make build            # debug build
make release          # optimized build -> target/release/dsync
make test
make check            # fmt + clippy
```

### Cross-compiling

Four targets are supported. Linux is cross-compiled with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) (Zig acts as the C cross-compiler for the
bundled SQLite) and produces fully static musl binaries. macOS targets are built natively with cargo, so run
those on a Mac. Windows is not supported.

```
make setup            # one-time: rustup targets + cargo-zigbuild (install zig first, e.g. brew install zig)
make all-targets      # everything into dist/
make linux-x86_64     # or: linux-arm64, macos-x86_64, macos-arm64
```

| Target         | Rust triple                  | Built with     |
|----------------|------------------------------|----------------|
| `linux-x86_64` | `x86_64-unknown-linux-musl`  | cargo-zigbuild |
| `linux-arm64`  | `aarch64-unknown-linux-musl` | cargo-zigbuild |
| `macos-x86_64` | `x86_64-apple-darwin`        | cargo          |
| `macos-arm64`  | `aarch64-apple-darwin`       | cargo          |

The GitHub Actions workflow in `.github/workflows/release.yml` runs the tests and builds all four targets on
every `v*` tag, attaching the binaries to a GitHub release.
