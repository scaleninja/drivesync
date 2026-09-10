# DriveSync (`dsync`)

A small, fast command-line tool that keeps a local folder and a Google Drive folder in sync.
Push local changes up, pull remote changes down, or just see what differs. Written in Rust,
modelled on [odeke-em/drive](https://github.com/odeke-em/drive).

- Home: <https://scaleninja.com/drivesync/>
- Source: <https://github.com/scaleninja/drivesync>
- License: [MIT](LICENSE)

> **Tested on:** macOS, with both a consumer `@gmail.com` account and a Google Workspace account.
> Linux and Windows builds are produced and smoke-tested in CI but have not been exercised against
> a live Drive; reports welcome.

```
dsync init ~/gdrive --remote-folder backups/lab --credentials ~/Downloads/client_secret.json
cd ~/gdrive
dsync diff      # what differs?
dsync push      # upload local changes (shows the plan, asks first)
dsync pull      # download remote changes
```

## Why this?

Compared to general-purpose tools such as rclone, `dsync` is deliberately narrow: one local folder,
one Drive folder, a single static Rust binary. What it does differently:

- Shows the plan and asks before changing anything; both-sides-changed is a conflict it will not
  overwrite without `--force`.
- Never deletes by default. With `--delete`, Drive entries go to the Drive trash and local files to
  the Trash on macOS or the Recycle Bin on Windows (see [Deleting](#deleting-with---delete)).
- Repeat runs are cheap: the remote index is kept current through the Drive Changes API instead of
  re-listing, and local MD5s are cached in a SQLite index keyed by size and mtime, like git.
- Content is compared by MD5 by default (Drive computes them server-side); `--fast` trusts equal
  size and mtime like rsync, `--verify` re-hashes every local file instead of trusting the cache.
- Every transfer is checked against Drive's MD5 after the fact; downloads land in a temp file and
  are renamed into place only once verified.
- Parallel transfers and hashing (8 streams by default, up to 64); resumable uploads that survive
  a restart.
- `.driveignore` with gitignore syntax, and `git`-style commands run from anywhere in the folder.

## Install

**Homebrew** (macOS and Linux):

```bash
brew install scaleninja/tap/drivesync
```

This adds the [scaleninja/tap](https://github.com/scaleninja/homebrew-tap) tap, so other
scaleninja tools then install with a plain `brew install <name>`. (If Homebrew refuses with an
"untrusted tap" error, run `brew trust scaleninja/tap` first.)

Or grab a prebuilt binary for Linux or macOS (x86_64 and arm64) from the
[releases page](https://github.com/scaleninja/drivesync/releases):

```bash
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset=linux-x86_64 ;;   Linux-aarch64) asset=linux-arm64 ;;
  Darwin-x86_64) asset=macos-x86_64 ;;   Darwin-arm64)  asset=macos-arm64 ;;
  *) echo "unsupported platform"; exit 1 ;;
esac
curl -fL -o /tmp/dsync "https://github.com/scaleninja/drivesync/releases/latest/download/dsync-$asset" \
  && chmod 755 /tmp/dsync && sudo mv /tmp/dsync /usr/local/bin/dsync
```

**Windows** (x86_64): download `dsync-windows-x86_64.exe` from the
[releases page](https://github.com/scaleninja/drivesync/releases), rename it to `dsync.exe`, and put
it in a folder on your `PATH`. See [Known limitations](#known-limitations) for Windows notes.

Or from source with a Rust toolchain: `git clone https://github.com/scaleninja/drivesync && cd drivesync && make install`.

## Setup

`dsync` talks to Drive with an OAuth client that **you** create, so no credentials are baked into
the binary. One-time steps in the [Google Cloud Console](https://console.cloud.google.com/)
(there is a [step-by-step walkthrough with screenshots](docs/SETUP.md)):

1. Create a project (or pick one).
2. **APIs & Services → Library**: enable the *Google Drive API*.
3. **APIs & Services → OAuth consent screen**: choose *External*, name the app (this name appears on
   the consent page), fill in the support emails, and add yourself under *Test users*.
4. **APIs & Services → Credentials → Create Credentials → OAuth client ID**: type *Desktop app*,
   then **Download JSON**.

Then initialize a folder. A browser opens for consent; the tool receives the code on a loopback port
and stores its tokens in `.gd/` inside the folder (mode 0600 on Unix, never uploaded).

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
| `--delete` | `push`/`pull` only. After the transfers, remove from the destination whatever no longer exists on the source: `push` moves Drive entries to the Drive trash; `pull` moves local files to the Trash on macOS or the Recycle Bin on Windows, and deletes them on Linux. Off by default; see [Deleting](#deleting-with---delete). |

Put gitignore-style patterns in `.driveignore` at the sync root to leave things out (`init` creates
one with `.DS_Store` and `._*`). `.gd/`, symlinks and non-regular files are never synced.

Exit status is 0 on success, 1 when `diff` found differences, and 2 on an error or when any
transfer failed. `PATH` is taken in its on-disk spelling, so on macOS `push Docs` and `push docs`
mean the same folder. Only one `dsync` command runs in a workspace at a time; a second one waits.

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

`+` create · `M` overwrite · `!` skipped for a structural reason · `C` conflict · `E` unreadable local file ·
`D` delete (only with `--delete`).

## What it will never do

- **Delete anything**, on either side, unless you ask for it with `--delete`. A file removed locally
  stays on Drive, and vice versa. See [Deleting](#deleting-with---delete) for what `--delete` does
  and the guards around it.
- **Overwrite a conflict** without `--force`, or a name collision (case or Unicode normalization,
  on filesystems that fold them) at all. While conflicts exist the
  prompt defaults to *no*, `--no-prompt` refuses to run, and end-of-input is never taken as *yes*.
- **Write stale data.** Every destination is re-checked right before it is written: a local file must
  still have the size and mtime the plan saw; a Drive file must still have the MD5, mtime, name and
  parent folder the plan saw, a file to be created must still be absent, and every folder new
  content goes into must still sit where the index placed it inside the sync tree. Anything that changed
  in between is reported and left alone. A local file that changes while it is being read or
  uploaded is reported as an error, and every upload is verified against the MD5 Drive computed.
- **Escape the sync folder.** Nothing is written through a symlink, into `.gd/`, over an ignored
  path, or over a non-regular file. Downloads go to a private temp file, are verified against Drive's
  MD5, then renamed into place keeping the existing permissions.
- **Create duplicates.** Drive is asked for a same-named entry immediately before every create, and
  creates use ids pre-generated by Drive, so neither a retry after a lost response nor a stale index
  makes a second copy or adopts someone else's file. Where Drive already holds duplicates, the oldest
  one consistently owns the path. Large uploads resume from the last byte Drive received, even across
  a restart: the session is kept in `.gd/` for as long as the file's size and mtime are unchanged.
- **Touch Google Docs, Sheets or Slides.** They have no binary content and are always skipped.

Files over 5 MB stream through resumable uploads; rate limits, network errors and stalled
connections are retried with backoff; a push or pull interrupted with Ctrl-C can simply be re-run.
If the remote folder is trashed or deleted on Drive, every command stops and says so. A damaged
`.gd/cache.db` is rebuilt automatically; it is only an index.

## Deleting with `--delete`

Like `rsync --delete`, `push --delete` and `pull --delete` remove from the destination whatever no
longer exists on the source, after the transfers. Where things go:

| | Destination | What `--delete` does |
|---|---|---|
| `push` | Google Drive | Moves entries to the **Drive trash**, restorable from Drive for a while. |
| `pull` on macOS | local folder | Moves files to the **Trash** (without Finder's *Put Back* entry; restore by dragging). If the Trash cannot be used, the file stays and the run reports a failure. |
| `pull` on Windows | local folder | Moves files to the **Recycle Bin**. If it cannot be used, the file stays and the run reports a failure. |
| `pull` on Linux | local folder | **Deletes permanently.** There is no trash integration on Linux. |

The guards:

- Deletions are listed in the plan as `D` lines with their own count, and the prompt defaults to
  *no* whenever any are present. `--no-prompt` applies them without asking; passing `--delete` to
  an unattended run means accepting whatever the plan would remove.
- An empty source is refused. If the source side holds nothing but the selected folder itself,
  `--delete` would clear the destination, so the command stops before doing anything.
- Deletions run last and only if every transfer succeeded. After any failure they are skipped and
  reported, so a partial run can never remove what it failed to copy.
- Every deletion is re-checked at the moment it happens. A Drive entry must still have the name,
  parent, content and modification time the plan saw and a Drive folder must be empty; a local file
  must still have the size and mtime the plan saw, nothing is followed through a symlink, and a
  local folder is only removed when empty. Anything that changed in between is left alone and
  reported.
- Never deleted: Google Docs, Sheets and Slides; names that collide only by case or Unicode
  normalization on a case-folding filesystem; anything below a path that is a folder on one side
  and a file on the other; anything below a local folder that could not be read; anything
  `.driveignore` excludes; and, on push, anything that exists locally in a form the sync does not
  cover (a symlink, a special file, or a folder the ignore rules prune). A folder that still
  contains such entries (a `.DS_Store`, a Google Doc, or content beyond the configured depth) is
  kept and reported as skipped, as is a Drive file owned by someone else, which only its owner can
  trash.

## Known limitations

- Two machines syncing the same Drive folder are not coordinated; there is no three-way merge. The
  Drive API has no conditional update, so the check that a remote file is still what the plan saw
  and the write (or, with `--delete`, the trash request) that follows are two requests; a write
  from elsewhere in that window is lost.
- Pull keeps no local backup: a local file it overwrites (only ever an older one, unless `--force`)
  is gone; one it removes with `--delete` goes to the Trash on macOS or the Recycle Bin on Windows
  and is deleted on Linux. Push
  replaces Drive content, which Drive keeps as a revision for a while, and trashes rather than deletes.
- A full listing holds every My Drive entry in memory while the tree is assembled.
- Another local process writing to the sync folder at the same moment as `dsync` is outside the
  supported threat model: destinations are checked immediately before each write, but not atomically
  with it.
- Keep the sync folder on a local disk. On NFS or SMB shares neither the workspace lock nor the
  SQLite index behaves reliably.
- The hash cache trusts an unchanged size and mtime, like git; use `--verify` after restoring files
  from a backup or when a tool rewrites files with their mtimes preserved.
- Windows: the token and cache files in `.gd/` are not given restrictive permissions (there are no
  Unix mode bits), so rely on your user profile's ACLs. Paths longer than 260 characters are not
  supported unless long paths are enabled system-wide. Drive names that Windows forbids in file
  names (`<>:"/\|?*`, or reserved names such as `CON`) cannot be pulled and are reported as
  failures. Only an x86_64 build is published.
- Filesystem nuances: on macOS and other case-folding filesystems, names that differ only by case or
  Unicode normalization are one local file, so such pairs are reported as collisions and never
  transferred (HFS+ stores accented names in NFD form, which can trigger this for names created on
  Drive). Filesystems with coarse timestamps (FAT, exFAT, some network shares) cause more hashing
  but no wrong decisions, since content is compared by MD5. Names longer than 255 bytes cannot be
  local files and are skipped.

## Building

```
make build | release | test | check     # check = fmt + clippy
make all-targets                        # Linux (static musl via cargo-zigbuild) + macOS into dist/
make setup                              # one-time: rustup targets + cargo-zigbuild (needs zig)
```

CI runs fmt, clippy, tests and a locked build on Linux, macOS and Windows for every push and pull
request; tags `v*` build all five targets (Linux and macOS on x86_64 and arm64, Windows on x86_64)
and publish them with a `SHA256SUMS` file.

To ship builds to your own users with a bundled OAuth client, set both variables at build time;
`init` then needs no `--client-id`/`--client-secret`. Google treats desktop client secrets as
non-confidential, but a bundled client shares one API quota, so keep such builds internal:

```
DSYNC_CLIENT_ID=... DSYNC_CLIENT_SECRET=... cargo build --release
```
