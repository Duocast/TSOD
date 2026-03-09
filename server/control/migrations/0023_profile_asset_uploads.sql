-- Profile asset upload sessions: tracks in-progress and verified asset uploads.
CREATE TABLE IF NOT EXISTS profile_asset_uploads (
  session_id      UUID PRIMARY KEY,
  user_id         UUID NOT NULL,
  server_id       UUID NOT NULL,
  purpose         TEXT NOT NULL,           -- 'profile_avatar' or 'profile_banner'
  mime_type       TEXT NOT NULL DEFAULT 'image/webp',
  byte_length     BIGINT NOT NULL DEFAULT 0,
  status          TEXT NOT NULL DEFAULT 'pending',  -- 'pending', 'uploaded', 'verified', 'rejected'
  asset_data      BYTEA NULL,              -- normalized/re-encoded image bytes (NULL until uploaded)
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at      TIMESTAMPTZ NOT NULL DEFAULT (now() + INTERVAL '10 minutes')
);

CREATE INDEX idx_profile_asset_uploads_user ON profile_asset_uploads(user_id);
CREATE INDEX idx_profile_asset_uploads_expires ON profile_asset_uploads(expires_at);

-- Profile custom status separate from the main profile row for rate-limit tracking.
ALTER TABLE user_profiles ADD COLUMN IF NOT EXISTS custom_status_text   TEXT NOT NULL DEFAULT '';
ALTER TABLE user_profiles ADD COLUMN IF NOT EXISTS custom_status_emoji  TEXT NOT NULL DEFAULT '';
ALTER TABLE user_profiles ADD COLUMN IF NOT EXISTS avatar_asset_url     TEXT NOT NULL DEFAULT '';
ALTER TABLE user_profiles ADD COLUMN IF NOT EXISTS banner_asset_url     TEXT NOT NULL DEFAULT '';
