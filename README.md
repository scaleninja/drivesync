# DriveSync (`dsync`)

A small, fast Rust CLI, modelled on [odeke-em/drive](https://github.com/odeke-em/drive), that pushes and
pulls a local directory to and from Google Drive and shows a diff of modification times between the two.

- Home: <https://scaleninja.com/drivesync/>
- Source: <https://github.com/scaleninja/drivesync>
- License: [MIT](LICENSE)

DriveSync (`dsync`) comes with ABSOLUTELY NO WARRANTY. This software is
distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY.

Usage:

## Usage

```
dsync init [DIR] [--remote-folder PATH] [--depth N] [--credentials FILE | --client-id ID --client-secret SECRET]
dsync push [PATH] [--force] [-y] [-j N] [--refresh] [--fast | --verify]
dsync pull [PATH] [--force] [-y] [-j N] [--refresh] [--fast | --verify]
dsync diff [PATH] [--refresh] [--fast | --verify] [-j N]
dsync status
dsync update-cache [--refresh]
dsync version
```

Run any command from anywhere inside the sync folder. `PATH` is relative to the current directory
and may be a file or a directory; it defaults to the current directory.

### Commands

| Command | What it does |
|---|---|
| `init [DIR]` | Turn `DIR` (default `.`) into a sync folder: authorize with Google in the browser, create or find the remote folder, write `DIR/.gd/` and a starter `.driveignore`. |
| `push [PATH]` | Upload local files that are new or newer than their remote copy. Shows the plan and asks first. |
| `pull [PATH]` | Download remote files that are new or newer than their local copy. Shows the plan and asks first. |
| `diff [PATH]` | List files that differ between local and remote, with both modification times. Changes nothing. Exit status 1 when differences exist, like `diff(1)`. |
| `status` | Show the local directory, remote folder, depth, cache state, ignore file and token expiry. |
| `update-cache` | Refresh the local index of the remote tree only, so a later `diff` or `push` starts faster. |
| `version` | Print the version. |

### Options

**`init`**

| Option | Meaning |
|---|---|
| `--remote-folder PATH` | Folder under *My Drive* to sync with, e.g. `backups/lab`. Created if missing. Default: the root of My Drive. |
| `--depth N` | How many levels deep to sync, counted from the sync root on both sides. `-1` (default) means unlimited; `1` means only the top level. Other values are rejected. |
| `--credentials FILE` | Path to the `client_secret.json` downloaded from Google Cloud Console. |
| `--client-id ID`, `--client-secret SECRET` | Alternative to `--credentials`. Also read from `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET`. |

**`push`, `pull`, `diff`**

| Option | Meaning |
|---|---|
| `--force` | Also overwrite files listed as conflicts because the destination is newer or has different content at the same mtime. Case collisions are never forced. Without it conflicts are never touched. |
| `-y`, `--no-prompt` | Apply the plan without asking. Refuses to run if the plan contains conflicts. (push, pull) |
| `-j N`, `--threads N` | Parallel transfer streams for push/pull, hashing threads for diff. Default 8, max 64. |
| `--refresh` | Ignore the cached remote index and re-list the whole remote tree. Use if the index looks wrong. |
| `--fast` | Use rsync's quick check instead of MD5 verification, see below. |
| `--verify` | Re-read every file that needs hashing instead of trusting the local hash cache. Use after restoring files from a backup or when an editor preserves mtimes. |

**`update-cache`**

| Option | Meaning |
|---|---|
| `--refresh` | Re-list the whole remote tree instead of applying only the changes since the last run. |

### What `--fast` does

By default every file present on both sides is verified by MD5: Drive supplies the remote MD5 for
free, and the local MD5 is computed once and cached against the file's size and mtime, so after the
first run only files whose stat changed are ever read. Like git's index, the cache trusts a file whose
size and mtime are unchanged; a file rewritten with its mtime deliberately restored is only noticed
with `--verify`, which bypasses the cache.

`--fast` switches to the quick check `rsync` uses without `-c`: a file whose **size and mtime match**
on both sides is trusted as identical without being read, and only files with equal size but differing
mtimes are hashed. It saves reading the whole tree once on a fresh workspace, at the cost of missing the
rare file whose content changed but whose size and mtime did not. Files with different sizes are known
to differ either way and are never read.

| | default | `--fast` |
|---|---|---|
| different size | different (no read) | different (no read) |
| same size, different mtime | hash and compare | hash and compare |
| same size, same mtime | hash and compare (cached after first time) | trusted identical |

### Environment

| Variable | Effect |
|---|---|
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | Defaults for `init --client-id` / `--client-secret`. |
| `DSYNC_CLIENT_ID`, `DSYNC_CLIENT_SECRET` | Build-time only: bake a client into the binary (see Building). |

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

### Workspace layout

`dsync init DIR` turns `DIR` into a sync root by creating `DIR/.gd/`:

| File | Contents |
|---|---|
| `.gd/config.json` | OAuth client id/secret, remote folder path and id, depth (mode `0600`) |
| `.gd/credentials.json` | access token, refresh token, expiry (mode `0600`) |
| `.gd/cache.db` | SQLite index of the remote tree plus the local hash cache |
| `.gd/lock` | advisory lock held during push and pull |
| `.driveignore` | gitignore-syntax patterns to leave out of the sync (created with `.DS_Store`) |

Every command may be run from any directory inside the sync root; `PATH` arguments are relative to
the current directory and may name a file or a folder (the folder itself is part of the comparison, so
an empty folder can be pushed). The same exclusions apply to both sides: `.gd/` at any depth, symlinks,
non-regular files (pipes, sockets, devices), `.driveignore` matches and leftover `.*.dsync-part` temp
files are never scanned locally and never pulled from Drive. `depth` counts levels from the sync root
on both sides.

### Authentication

`init` runs the OAuth 2.0 installed-app flow with PKCE: it listens on a random loopback port, opens the
consent URL in the browser (with a random `state` nonce that the redirect must echo back and an S256
code challenge), exchanges the code for tokens and stores them. Nothing is written to `.gd/` until
Google has accepted the authorization; state files are written atomically with mode `0600`. From then on the access token is refreshed automatically whenever it is
within 60 s of expiry or the API answers 401. Worker threads share one token store, so concurrent
401s trigger a single refresh. The Drive scope is `https://www.googleapis.com/auth/drive`, which is
needed to see files that were not created by `dsync`.

### The remote index

Listing a Drive tree costs one request per folder, so `dsync` keeps an index of the remote tree in
`cache.db` and updates it incrementally:

- **First run** (or `--refresh`, or after `depth` changes): one paginated query lists every file in
  My Drive with its parent id (1000 per page) and the tree is assembled locally. A Drive Changes API
  token is recorded before listing so nothing that happens meanwhile is missed.
- **Every later run**: `changes.list` returns only what changed since the token; renames, moves,
  trashing and deletions are applied to the index, folders that newly appeared inside the tree are
  listed once. If the incremental update fails the tool warns and falls back to a full listing.
- **After every completed upload or folder creation** the worker records the result immediately, so
  a push interrupted with Ctrl-C is resumable on the next run without waiting for the Changes feed.
  Folders whose contents still need listing are recorded in the database too, so a crash between two
  steps of a refresh is finished by the next run rather than forgotten.
- When Drive holds several items with one name in a folder the index keeps the first it saw; a
  change to a duplicate never silently swaps the identity behind a path.
- `dsync update-cache` refreshes the index without doing anything else.

The database is opened in WAL mode with a busy timeout, so several instances can read and write it
concurrently. Push, pull and init hold `.gd/lock` exclusively; diff and update-cache hold it shared,
so a refresh never interleaves with a transfer.

### Deciding what changed

Each side is a map from relative path to (mtime, size, MD5). Local entries come from a filesystem
walk (stat only). Remote entries come from the index, where Drive has already supplied the MD5 of
every binary file. A pair is then compared, cheapest check first:

1. Different sizes: different, nothing is read.
2. Otherwise the local MD5 is needed. It comes from the `local_hashes` table if the file's size and
   mtime still match the cached stat, and is computed otherwise (in parallel, 1 MiB buffer) and
   cached. Hashes are also recorded after every upload and download, so a freshly synced file is never
   re-read.
3. Equal MD5: identical, whatever the mtimes say.
4. Different MD5: the newer side (1 s tolerance) is the change. Equal mtimes with different content
   is a **conflict**.

`--fast` replaces steps 2 and 3 with rsync's quick check: equal size and equal mtime is trusted
without reading the file, and only equal size with differing mtimes is hashed. `--verify` ignores the
hash cache and re-reads every file that needs hashing. A local file that cannot be read is reported
as an error, never treated as identical, and makes the command exit non-zero.

On a case-insensitive filesystem (the macOS and Windows defaults) names that differ only by case, and
everything below such folders, are reported as conflicts and never transferred in either direction,
because they would map onto one local file.

Uploads set Drive's `modifiedTime` to the local mtime and downloads set the local mtime to Drive's
`modifiedTime`, so both sides agree after a transfer and `dsync diff` reports nothing.

### Push, pull and diff

`push` and `pull` first build a plan and print it, one line per path, then count lines and a
prompt (`--no-prompt` / `-y` skips it):

```
+ photos/2026/
+ photos/2026/a.jpg  1,234,567 B
M docs/report.txt  12,340 B, local newer
! docs/Budget  skipped: remote is a Google-native document; not overwritten
C notes.txt  conflict: remote is newer
Addition count 2 src: 1,234,567 B
Modification count 1 src: 12,340 B
Skip count 1
Conflict count 1
Proceed with the changes? [Y/n]:
```

- **`+`** will be created on the destination, **`M`** will be overwritten because the source is newer
  (or `--force` was given), **`!`** is left alone for a structural reason (folder/file mismatch,
  Google-native document), **`C`** is a conflict, **`E`** is a local file that could not be read.
- **Conflicts** are never transferred. While any exist the prompt defaults to *no* and `--no-prompt`
  refuses to run at all. Inspect with `dsync diff`, fix by hand, or re-run with `--force` (case
  collisions cannot be forced). End of input at the prompt counts as *no*.
- **Every write is re-validated at execution time.** Before a downloaded file replaces a local one,
  the local file must still have exactly the size and mtime the plan saw (or still be absent); before
  an update is uploaded, the Drive file must still have the MD5, mtime, name and parent folder the plan
  saw, so a file moved out of the sync folder is never written to. Anything that changed in between is
  reported as failed and left alone; re-run to re-plan. No write ever goes through a symlink (not even
  when a selected path lies below one), into `.gd/`, or over a non-regular file.
- **Nothing is ever deleted** on either side; a file removed locally stays on Drive and vice versa.
- **Google-native documents** (Docs, Sheets, Slides, ...) have no binary content and are listed as
  skipped; a local file with the same name is never uploaded over one.

`diff` lists the same markers with both modification times and does not change anything:

```
M docs/report.txt  local newer  local: 2026-09-08T10:00:00.000Z  remote: 2026-09-08T09:00:00.000Z
+ only_here.txt  local only, 1,024 B
- only_there.txt  remote only, 2,048 B
3 file(s) differ: 1 local newer, 1 local only, 1 remote only
```

### Transfers

- Missing remote folders are created level by level, all folders of one level in parallel, so a
  deep tree costs a few round trips rather than one per folder. Then files transfer on `--threads`
  (`-j`) workers, default 8, max 64, over a shared HTTP/2 connection. A spinner shows progress.
- Files up to 5 MB are uploaded in one multipart request; larger files stream from disk through a
  resumable upload session, so memory use does not grow with file size. Drive commits a file only
  when the last byte arrives, so an interrupted upload leaves nothing behind.
- Downloads stream to a freshly created `.name.dsync-part` temp file (mode `0600`, never through a
  link), are verified against Drive's MD5, and only then renamed into place with the remote mtime
  applied. An existing file keeps its permission bits; a new file gets `0644`.
- Every create uses an id pre-generated by Drive, so a retried create can never make a duplicate or
  adopt someone else's same-name file: if the first attempt went through, Drive answers 409 and the
  file is fetched. A large upload that fails mid-way asks its resumable session how much arrived and
  continues from there.
- Rate-limit (403/429), server (5xx) and network errors are retried with exponential backoff and
  jitter. One failed file does not stop the others; the exit code is non-zero if any failed.
- There is no overall request timeout, so multi-gigabyte transfers are fine; dead connections are
  detected by the connect timeout and TCP keepalive.

### Names and edge cases

Spaces, quotes, `%`, `_`, backslashes and Unicode in file and folder names are handled on both
sides. Drive names that cannot exist locally (containing `/`, or `.`/`..`) and local names that are
not valid UTF-8 are reported and skipped. Control characters in names are escaped before printing.

### Known limitations

- Two machines syncing the same remote folder are not coordinated with each other; the newer-side rule
  and the execution-time checks limit the damage, but there is no three-way merge and no history.
- Drive has no conditional update, so the pre-upload check narrows the window in which another editor
  can change a remote file but cannot close it.
- A full listing holds every My Drive entry in memory while the tree is assembled.
- Unicode normalization differences (NFC vs NFD) are not detected as collisions.
- Google-native documents are never downloaded; nothing is ever deleted on either side.

## Building

```
make build            # debug build
make release          # optimized build -> target/release/dsync
make test
make check            # fmt + clippy
```

### Bundling an OAuth client into a build

The public source ships without any Google credentials, so users create their own OAuth client
(see Setup). An organisation distributing `dsync` to its own users can bake a client into the binary
at build time; `dsync init` then works with no `--client-id`/`--client-secret`:

```
DSYNC_CLIENT_ID=... DSYNC_CLIENT_SECRET=... cargo build --release
```

Both variables must be set together; a partial pair is ignored. Explicit flags, environment variables
and `--credentials` still take precedence. Google treats
installed-app client secrets as non-confidential, but a bundled client shares one API quota and one
consent-screen identity across everyone using that build, so keep such builds internal.

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

`.github/workflows/ci.yml` runs `cargo fmt --check`, `clippy -D warnings`, the tests and a locked release
build on Linux and macOS for every push and pull request, plus a RustSec advisory scan.
`.github/workflows/release.yml` repeats the checks and builds all four targets on every `v*` tag,
attaching the binaries and a `SHA256SUMS` file to a GitHub release.
