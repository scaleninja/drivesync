# DriveSync (`dsync`)

A small, fast command-line tool that keeps a local folder and a Google Drive folder in sync.
Push local changes up, pull remote changes down, or just see what differs. Written in Rust,
modelled on [odeke-em/drive](https://github.com/odeke-em/drive).

- Home: <https://scaleninja.com/drivesync/> · Source: <https://github.com/scaleninja/drivesync> · License: [MIT](LICENSE)

```
dsync init ~/gdrive --remote-folder backups/lab --credentials ~/Downloads/client_secret.json
cd ~/gdrive
dsync diff      # what differs?
dsync push      # upload local changes (shows the plan, asks first)
dsync pull      # download remote changes
```

## Install

Prebuilt binaries for Linux and macOS (x86_64 and arm64) are on the
[releases page](https://github.com/scaleninja/drivesync/releases).

```bash
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset=linux-x86_64 ;;   Linux-aarch64) asset=linux-arm64 ;;
  Darwin-x86_64) asset=macos-x86_64 ;;   Darwin-arm64)  asset=macos-arm64 ;;
  *) echo "unsupported platform"; exit 1 ;;
esac
curl -fL -o /tmp/dsync "https://github.com/scaleninja/drivesync/releases/latest/download/dsync-$asset" \
  && chmod 755 /tmp/dsync && sudo mv /tmp/dsync /usr/local/bin/dsync
```

Or from source with a Rust toolchain: `git clone https://github.com/scaleninja/drivesync && cd drivesync && make install`.

## Setup

`dsync` talks to Drive with an OAuth client that **you** create, so no credentials are baked into
the binary. One-time steps in the [Google Cloud Console](https://console.cloud.google.com/):

1. Create a project (or pick one).
2. **APIs & Services → Library**: enable the *Google Drive API*.
3. **APIs & Services → OAuth consent screen**: choose *External*, name the app (this name appears on
   the consent page), fill in the support emails, and add yourself under *Test users*.
4. **APIs & Services → Credentials → Create Credentials → OAuth client ID**: type *Desktop app*,
   then **Download JSON**.

Then initialize a folder. A browser opens for consent; the tool receives the code on a loopback port
and stores its tokens in `.gd/` inside the folder (mode 0600, never uploaded).

```bash
dsync init ~/gdrive --remote-folder backups/lab --credentials ~/Downloads/client_secret.json
```

> Google expires refresh tokens after 7 days for External apps still in *Testing*. If you are asked
> to re-run `init` every week, click **Publish app** on the consent screen (no verification is needed
> for personal use).

## Usage

Run any command from anywhere inside the sync folder. `PATH` is relative to the current directory
and may be a file or a folder; it defaults to `.`.

| Command | Does |
|---|---|
| `init [DIR] --remote-folder P --credentials F` | Authorize and turn `DIR` into a sync folder mirroring `My Drive/P`. `--depth N` limits levels (`-1` = unlimited). |
| `push [PATH]` | Upload files that are new or newer locally. Shows the plan and asks first. |
| `pull [PATH]` | Download files that are new or newer on Drive. Shows the plan and asks first. |
| `diff [PATH]` | List what differs, with both modification times. Changes nothing; exit status 1 if anything differs. |
| `status` | Local folder, remote folder, depth, cache state, ignore file, filesystem kind, token expiry. |
| `update-cache` | Refresh the index of the remote tree only. |
| `version` | Print the version. |

Options for `push`, `pull` and `diff`:

| Option | Effect |
|---|---|
| `-y`, `--no-prompt` | Apply without asking. Refused if the plan contains conflicts. |
| `--force` | Also overwrite conflicts where the destination is newer or has different content at the same mtime. Case collisions are never forced. |
| `-j N`, `--threads N` | Parallel streams (default 8, max 64). |
| `--refresh` | Ignore the cached index and re-list the whole remote tree. |
| `--fast` | rsync-style quick check: equal size and mtime is trusted without reading the file. |
| `--verify` | Re-read every file that needs hashing instead of trusting the local hash cache. |

Put gitignore-style patterns in `.driveignore` at the sync root to leave things out (`init` creates
one with `.DS_Store`). `.gd/`, symlinks and non-regular files are never synced.

## How it decides what changed

Each side is a set of paths with size, mtime and MD5. Drive computes MD5s server-side and returns
them in listings, so the remote side is free. Files are compared cheapest check first:

1. Different sizes → different, nothing is read.
2. Otherwise the local MD5 is compared. It is computed once and cached against the file's size and
   mtime, so after the first run only files whose stat changed are read again (`--verify` bypasses
   the cache; `--fast` skips hashing when size and mtime match).
3. Equal MD5 → identical, whatever the mtimes say.
4. Different MD5 → the newer side (1 s tolerance) wins. Same mtime with different content is a
   **conflict**.

After a transfer both sides carry the same mtime, so `diff` reports nothing until something changes.
The remote tree is indexed once in full, then kept current through the Drive Changes API, so
repeated runs do not re-list Drive.

Push and pull print one line per change and ask before doing anything:

```
+ photos/2026/
+ photos/2026/a.jpg  1,234,567 B
M docs/report.txt  12,340 B, local newer
! docs/Budget  skipped: remote is a Google-native document; not overwritten
C notes.txt  conflict: remote is newer
Addition count 2 src: 1,234,567 B
Modification count 1 src: 12,340 B
Proceed with the changes? [Y/n]:
```

`+` create · `M` overwrite · `!` skipped for a structural reason · `C` conflict · `E` unreadable local file.

## What it will never do

- **Delete anything**, on either side. A file removed locally stays on Drive, and vice versa.
- **Overwrite a conflict** without `--force`, or a name collision (case or Unicode normalization,
  on filesystems that fold them) at all. While conflicts exist the
  prompt defaults to *no*, `--no-prompt` refuses to run, and end-of-input is never taken as *yes*.
- **Write stale data.** Every destination is re-checked right before it is written: a local file must
  still have the size and mtime the plan saw; a Drive file must still have the MD5, mtime, name and
  parent folder the plan saw. Anything that changed in between is reported and left alone.
- **Escape the sync folder.** Nothing is written through a symlink, into `.gd/`, over an ignored
  path, or over a non-regular file. Downloads go to a private temp file, are verified against Drive's
  MD5, then renamed into place keeping the existing permissions.
- **Create duplicates.** Creates use ids pre-generated by Drive, so a retry after a lost response
  can never make a second copy or adopt someone else's same-name file. Large uploads resume from the
  last byte Drive received.
- **Touch Google Docs, Sheets or Slides.** They have no binary content and are always skipped.

Files over 5 MB stream through resumable uploads; rate limits and network errors are retried with
backoff; a push interrupted with Ctrl-C can simply be re-run.

## Known limitations

- Two machines syncing the same Drive folder are not coordinated; there is no three-way merge.
- A full listing holds every My Drive entry in memory while the tree is assembled.
- Another local process writing to the sync folder at the same moment as `dsync` is outside the
  supported threat model: destinations are checked immediately before each write, but not atomically
  with it.
- The hash cache trusts an unchanged size and mtime, like git; use `--verify` after restoring files
  from a backup or when a tool rewrites files with their mtimes preserved.

## Building

```
make build | release | test | check     # check = fmt + clippy
make all-targets                        # Linux (static musl via cargo-zigbuild) + macOS into dist/
make setup                              # one-time: rustup targets + cargo-zigbuild (needs zig)
```

Windows is not supported. CI runs fmt, clippy, tests and a locked build on every push and pull
request; tags `v*` build all four targets and publish them with a `SHA256SUMS` file.

To ship builds to your own users with a bundled OAuth client, set both variables at build time;
`init` then needs no `--client-id`/`--client-secret`. Google treats desktop client secrets as
non-confidential, but a bundled client shares one API quota, so keep such builds internal:

```
DSYNC_CLIENT_ID=... DSYNC_CLIENT_SECRET=... cargo build --release
```
