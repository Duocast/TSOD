# Client auto-update (axoupdater + GitHub Releases)

The desktop client (`client/`) uses `axoupdater` in-app with app id `vp-client`.

## Versioning model

- **Canonical app/update version**: `CARGO_PKG_VERSION` (`APP_VERSION`).
  - Must be semver-compatible.
  - For rapid releases, use pre-release tags such as:
    - `0.1.0-dev.20260312.t090543`
- **Build identity**: `VP_CLIENT_BUILD_VERSION` (`BUILD_VERSION`).
  - Generated from build timestamp in `client/build.rs`.
  - Display/debug only; **not** used as updater release identity.

## Distribution requirements

Automatic install requires a `cargo-dist` install receipt on disk.

- Supported path: release artifacts produced/distributed with `cargo-dist` and published to GitHub Releases.
- Unsupported path: local/dev/manual zip installs that do not have a receipt.

When unsupported, the client shows a friendly message:

> Automatic updates are unavailable for this install type

## Runtime behavior

- If `settings.check_for_updates` is enabled, app startup does a background **check only**.
- Startup does not auto-install updates.
- About window has:
  - Check for updates
  - Install update (only when an update is available)
