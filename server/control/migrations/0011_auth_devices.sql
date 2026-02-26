CREATE TABLE IF NOT EXISTS auth_users (
  user_id      UUID PRIMARY KEY,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS auth_devices (
  device_id    UUID PRIMARY KEY,
  user_id      UUID NOT NULL REFERENCES auth_users(user_id) ON DELETE CASCADE,
  pubkey       BYTEA NOT NULL UNIQUE,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen    TIMESTAMPTZ NOT NULL DEFAULT now(),
  revoked_at   TIMESTAMPTZ NULL
);
