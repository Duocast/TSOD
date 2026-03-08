CREATE TABLE IF NOT EXISTS user_profiles (
  user_id         UUID PRIMARY KEY,
  server_id       UUID NOT NULL,
  display_name    TEXT NOT NULL DEFAULT '',
  description     TEXT NOT NULL DEFAULT '',
  accent_color    INTEGER NOT NULL DEFAULT 0,
  avatar_asset_id UUID NULL,
  banner_asset_id UUID NULL,
  links           JSONB NOT NULL DEFAULT '[]',
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_user_profiles_server ON user_profiles(server_id);

-- Badges are server-managed, not user-editable
CREATE TABLE IF NOT EXISTS user_badges (
  user_id    UUID NOT NULL,
  badge_id   TEXT NOT NULL,
  server_id  UUID NOT NULL,
  granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, badge_id)
);

CREATE TABLE IF NOT EXISTS badge_definitions (
  id          TEXT PRIMARY KEY,
  server_id   UUID NOT NULL,
  label       TEXT NOT NULL,
  icon_url    TEXT NOT NULL DEFAULT '',
  tooltip     TEXT NOT NULL DEFAULT '',
  position    INTEGER NOT NULL DEFAULT 0,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
