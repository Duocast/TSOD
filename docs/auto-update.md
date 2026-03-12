# Client auto-update (axoupdater + GitHub Releases)

The desktop client (`client/`) supports two update paths:

1. **`cargo-dist` installs** (all platforms): uses `axoupdater` with app id `vp-client`.
2. **Portable Windows EXE installs**: falls back to GitHub Releases and stages an in-place EXE swap on app exit.

## Versioning model

- **Canonical app/update version**: `CARGO_PKG_VERSION` (`APP_VERSION`).
  - Must be semver-compatible.
  - For rapid releases, use pre-release tags such as:
    - `0.1.0-dev.20260312.t090543`
- **Build identity**: `VP_CLIENT_BUILD_VERSION` (`BUILD_VERSION`).
  - Generated from build timestamp in `client/build.rs`.
  - Display/debug only; **not** used as updater release identity.

## Distribution requirements

### `cargo-dist` path (preferred)

Automatic install requires a `cargo-dist` install receipt on disk.

- Supported path: release artifacts produced/distributed with `cargo-dist` and published to GitHub Releases.
- Unsupported path: local/dev/manual installs that do not have a receipt.

### Portable Windows EXE path

When no `cargo-dist` receipt exists on Windows, the client:

- checks `https://api.github.com/repos/Duocast/TSOD/releases/latest` (API form of `https://github.com/Duocast/TSOD/releases`),
- picks a Windows `.exe` asset,
- compares release tag version against `CARGO_PKG_VERSION`,
- downloads the newer EXE beside the running executable,
- and starts a small swap script that waits for app exit, replaces the EXE, and relaunches.

This keeps distribution portable while still providing an end-user “Check for updates / Install update” flow.

## Runtime behavior

- If `settings.check_for_updates` is enabled, app startup does a background **check only**.
- Startup does not auto-install updates.
- About window has:
  - Check for updates
  - Install update (only when an update is available)

## Operational note for portable builds

For portable update reliability, publish a Windows `.exe` asset on every GitHub Release tag and keep tag names semver-like (for example `v0.1.3` or `0.1.3`).
