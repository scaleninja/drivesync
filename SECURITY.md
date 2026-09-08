# Security Policy

DriveSync (`dsync`) is a ScaleNinja tool. This file covers the CLI in this repository and the
release artifacts built from it. ScaleNinja's full security policy, which also applies here, is
at <https://scaleninja.com/security>.

## Reporting a vulnerability

Please **do not open a public GitHub issue** for security problems.

Email **security@scaleninja.com** with as much of the following as you can:

- The `dsync` version (`dsync version`), platform, and how you installed it (release binary,
  Homebrew, or from source).
- A description of the issue and its impact.
- Steps to reproduce, ideally with a minimal proof of concept.
- Your name or handle if you'd like to be credited.

You can request a PGP key from the same address if you'd like to encrypt your report.

## What to expect

- Acknowledgement within two business days.
- Honest updates on whether the issue qualifies, the planned fix, and the timeline.
- A fix shipped as a new release, with credit in the release notes if you want it.
- No legal action against good-faith researchers who follow this policy.

We ask that you give us up to 90 days to ship a fix before public disclosure, and that you don't
access or modify data that isn't your own. We don't run a paid bug-bounty program.

## Supported versions

Only the latest release on the [releases page](https://github.com/scaleninja/drivesync/releases)
receives security fixes. Please upgrade before reporting.

## Scope

In scope:

- The `dsync` binary and everything under `src/`.
- Handling of OAuth credentials and tokens stored in the sync folder's `.gd/` directory.
- Sync logic that could delete, overwrite, or leak files beyond the configured folder.
- The release pipeline (`.github/workflows/`), published binaries, and the Homebrew formula in
  [scaleninja/homebrew-tap](https://github.com/scaleninja/homebrew-tap).

Out of scope unless chained into real impact:

- Issues in Google Drive, the Google OAuth flow, or other third-party services (report those to
  the vendor).
- Anything requiring an attacker who already has your local user account, since `.gd/` tokens
  are readable by the user who owns the sync folder by design.
- Google's refresh-token expiry for OAuth apps in "Testing" status.
- Reports from automated scanners without a demonstrated impact.

## How `dsync` handles your data

- All work happens on your machine. Files and metadata go only to the Google Drive API over
  HTTPS, using an OAuth client that you create; no credentials are baked into the binary.
- Tokens are written to `.gd/` with mode 0600 and are never uploaded.
- There is no telemetry, analytics, or crash reporting.
- Release binaries are built by GitHub Actions from the tagged commit and published with a
  `SHA256SUMS` file. Dependencies are pinned in `Cargo.lock` and checked against the RustSec
  advisory database in CI.
