-- Core channels
CREATE TABLE IF NOT EXISTS channels (
  id            UUID PRIMARY KEY,
  server_id     UUID NOT NULL,
  name          TEXT NOT NULL,
  parent_id     UUID NULL REFERENCES channels(id) ON DELETE SET NULL,
  max_members   INTEGER NULL,
  max_talkers   INTEGER NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Membership (authoritative)
CREATE TABLE IF NOT EXISTS channel_members (
  channel_id    UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id       UUID NOT NULL,
  display_name  TEXT NOT NULL,
  muted         BOOLEAN NOT NULL DEFAULT FALSE,
  deafened      BOOLEAN NOT NULL DEFAULT FALSE,
  joined_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (channel_id, user_id)
);

-- Roles and permissions
CREATE TABLE IF NOT EXISTS roles (
  id            TEXT PRIMARY KEY,              -- e.g. "admin", "member", "mod"
  server_id     UUID NOT NULL,
  name          TEXT NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Capability grants/denies per role
CREATE TABLE IF NOT EXISTS role_caps (
  role_id       TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  cap           TEXT NOT NULL,
  effect        TEXT NOT NULL CHECK (effect IN ('grant', 'deny')),
  PRIMARY KEY (role_id, cap, effect)
);

CREATE TABLE IF NOT EXISTS user_roles (
  server_id     UUID NOT NULL,
  user_id       UUID NOT NULL,
  role_id       TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  PRIMARY KEY (server_id, user_id, role_id)
);

-- Per-channel overrides for a user (grant/deny)
CREATE TABLE IF NOT EXISTS channel_overrides (
  channel_id    UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id       UUID NOT NULL,
  cap           TEXT NOT NULL,
  effect        TEXT NOT NULL CHECK (effect IN ('grant', 'deny')),
  PRIMARY KEY (channel_id, user_id, cap, effect)
);

-- Transactional outbox for events (Presence, Chat, Moderation)
CREATE TABLE IF NOT EXISTS outbox_events (
  id            TEXT PRIMARY KEY,              -- ULID string
  server_id     UUID NOT NULL,
  topic         TEXT NOT NULL,                 -- e.g. "presence"
  key           TEXT NOT NULL,                 -- e.g. channel_id
  payload       JSONB NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  published_at  TIMESTAMPTZ NULL
);

-- Audit log
CREATE TABLE IF NOT EXISTS audit_log (
  id            TEXT PRIMARY KEY,              -- ULID string
  server_id     UUID NOT NULL,
  actor_user_id UUID NULL,                     -- may be null for system actions
  action        TEXT NOT NULL,                 -- e.g. "channel.create", "member.mute"
  target_type   TEXT NOT NULL,                 -- e.g. "channel", "user"
  target_id     TEXT NOT NULL,
  context       JSONB NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
