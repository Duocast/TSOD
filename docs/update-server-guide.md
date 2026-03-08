# TSOD Secure Update Server Guide

This guide describes how to host TSOD update artifacts over HTTPS so the client can check for updates and download new executables directly.

## 1) Threat model and requirements

- Serve updates **only over HTTPS**.
- Keep update metadata (`manifest.json`) and binaries immutable/versioned.
- Restrict write access to update storage (CI/release automation only).
- Prefer a CDN/object storage origin (S3, R2, GCS) behind TLS.

## 2) Manifest format

TSOD expects a JSON manifest at the configured `update_manifest_url`.

```json
{
  "version": "0.2.0",
  "notes": "Bug fixes and improved audio recovery.",
  "download_url": "https://updates.example.com/tsod/0.2.0/tsod-0.2.0-windows-x64.exe",
  "page_url": "https://updates.example.com/tsod/0.2.0/release-notes.html"
}
```

Fields:
- `version` (required): semantic-style version string.
- `download_url` (required): direct HTTPS URL to installer/executable.
- `page_url` (optional): release notes page (falls back to `download_url` if missing).
- `notes` (optional): short one-paragraph summary shown in-app.

## 3) Nginx hardened example

```nginx
server {
  listen 443 ssl http2;
  server_name updates.example.com;

  ssl_certificate     /etc/letsencrypt/live/updates.example.com/fullchain.pem;
  ssl_certificate_key /etc/letsencrypt/live/updates.example.com/privkey.pem;
  ssl_protocols TLSv1.2 TLSv1.3;
  ssl_ciphers HIGH:!aNULL:!MD5;
  add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

  root /srv/updates;

  location = /tsod/stable/manifest.json {
    add_header Cache-Control "no-cache, no-store, must-revalidate";
    default_type application/json;
    try_files $uri =404;
  }

  location /tsod/ {
    add_header Cache-Control "public, max-age=31536000, immutable";
    types { application/octet-stream exe msi AppImage zip dmg; }
    try_files $uri =404;
  }
}
```

Recommended layout:

```
/srv/updates/tsod/stable/manifest.json
/srv/updates/tsod/0.2.0/tsod-0.2.0-windows-x64.exe
/srv/updates/tsod/0.2.0/tsod-0.2.0-linux-x86_64.AppImage
/srv/updates/tsod/0.2.0/release-notes.html
```

## 4) Release publishing workflow

1. Build and sign artifacts in CI.
2. Upload artifacts to versioned paths.
3. Upload release notes page.
4. Update `stable/manifest.json` last.
5. Optionally invalidate CDN cache for `manifest.json`.

## 5) TSOD client behavior

- On startup, if `check_for_updates` is enabled, TSOD fetches `update_manifest_url`.
- Users can force a check from **About → Check for updates now**.
- If a newer version is available, TSOD shows a popup with:
  - release notes link,
  - **Get update** action,
  - and download status/toast when complete.
- Downloaded files are saved under the user Downloads folder in `tsod-updates/`.

## 6) Operational hardening checklist

- [ ] TLS cert auto-renewal configured.
- [ ] Access logs and anomaly alerts enabled.
- [ ] Update storage write access limited to CI deploy identity.
- [ ] Artifacts scanned and signed before upload.
- [ ] Manifest changed only after artifacts are reachable.
- [ ] Rollback plan: repoint manifest to previous known-good version.
